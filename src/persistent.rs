//! Persistent/Durable buffer implementation with crash recovery.
//!
//! This module provides a durable circular buffer that persists data to disk
//! using memory-mapped files, ensuring data survives process restarts.

use crate::{BufferError, BufferResult, Config};
use memmap2::{MmapMut, MmapOptions};
use parking_lot::{Mutex, RwLock};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Sync mode for durability guarantees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// No explicit syncing - relies on OS page cache (fastest, least durable)
    NoSync,
    /// Sync periodically at the specified interval
    Periodic(Duration),
    /// Sync after every write operation (slowest, most durable)
    EveryWrite,
}

impl Default for SyncMode {
    fn default() -> Self {
        SyncMode::Periodic(Duration::from_secs(1))
    }
}

/// Configuration for the persistent buffer.
#[derive(Clone, Debug)]
pub struct PersistentConfig {
    /// Base buffer configuration
    pub base: Config,
    /// Path to the data file
    pub data_path: PathBuf,
    /// Sync mode for durability
    pub sync_mode: SyncMode,
    /// Maximum file size in bytes (default: 1GB)
    pub max_file_size: usize,
    /// Whether to recover data on startup
    pub recover_on_startup: bool,
}

impl PersistentConfig {
    /// Create a new persistent config with the given data path.
    pub fn new<P: AsRef<Path>>(data_path: P) -> Self {
        Self {
            base: Config::default(),
            data_path: data_path.as_ref().to_path_buf(),
            sync_mode: SyncMode::default(),
            max_file_size: 1024 * 1024 * 1024, // 1GB
            recover_on_startup: true,
        }
    }

    /// Set the base configuration.
    pub fn with_base_config(mut self, config: Config) -> Self {
        self.base = config;
        self
    }

    /// Set the sync mode.
    pub fn with_sync_mode(mut self, mode: SyncMode) -> Self {
        self.sync_mode = mode;
        self
    }

    /// Set the maximum file size.
    pub fn with_max_file_size(mut self, size: usize) -> Self {
        self.max_file_size = size;
        self
    }

    /// Set whether to recover data on startup.
    pub fn with_recover_on_startup(mut self, recover: bool) -> Self {
        self.recover_on_startup = recover;
        self
    }

    /// Validate the configuration.
    pub fn validate(&self) -> BufferResult<()> {
        self.base.validate()?;
        if self.max_file_size == 0 {
            return Err(BufferError::InvalidConfiguration(
                "max_file_size must be greater than 0".to_string(),
            ));
        }
        Ok(())
    }
}

/// Header stored at the beginning of the data file.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct FileHeader {
    /// Magic bytes for file identification
    magic: [u8; 8],
    /// Version of the file format
    version: u32,
    /// Head position (read pointer)
    head: u64,
    /// Tail position (write pointer)
    tail: u64,
    /// Number of items in the buffer
    item_count: u64,
    /// Checksum of the header for integrity
    checksum: u32,
}

impl FileHeader {
    const MAGIC: [u8; 8] = *b"ELASTICQ";
    const VERSION: u32 = 1;
    const SIZE: usize = 64; // Fixed header size

    fn new() -> Self {
        Self {
            magic: Self::MAGIC,
            version: Self::VERSION,
            head: 0,
            tail: 0,
            item_count: 0,
            checksum: 0,
        }
    }

    fn calculate_checksum(&self) -> u32 {
        // Simple checksum: XOR of all fields
        let mut sum: u32 = 0;
        for b in &self.magic {
            sum = sum.wrapping_add(*b as u32);
        }
        sum = sum.wrapping_add(self.version);
        sum = sum.wrapping_add((self.head & 0xFFFFFFFF) as u32);
        sum = sum.wrapping_add((self.head >> 32) as u32);
        sum = sum.wrapping_add((self.tail & 0xFFFFFFFF) as u32);
        sum = sum.wrapping_add((self.tail >> 32) as u32);
        sum = sum.wrapping_add((self.item_count & 0xFFFFFFFF) as u32);
        sum = sum.wrapping_add((self.item_count >> 32) as u32);
        sum
    }

    fn with_checksum(mut self) -> Self {
        self.checksum = self.calculate_checksum();
        self
    }

    fn verify_checksum(&self) -> bool {
        self.checksum == self.calculate_checksum()
    }

    fn verify_magic(&self) -> bool {
        self.magic == Self::MAGIC
    }
}

/// Entry stored in the write-ahead log.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct LogEntry<T> {
    /// Sequence number for ordering
    sequence: u64,
    /// The actual data
    data: T,
    /// Entry checksum
    checksum: u32,
}

impl<T: Serialize> LogEntry<T> {
    fn new(sequence: u64, data: T) -> BufferResult<Self> {
        let mut entry = Self {
            sequence,
            data,
            checksum: 0,
        };
        entry.checksum = entry.calculate_checksum()?;
        Ok(entry)
    }

    fn calculate_checksum(&self) -> BufferResult<u32> {
        let bytes = bincode::serialize(&self.data)
            .map_err(|e| BufferError::Other(format!("Serialization error: {}", e)))?;
        let mut sum: u32 = (self.sequence & 0xFFFFFFFF) as u32;
        sum = sum.wrapping_add((self.sequence >> 32) as u32);
        for b in &bytes {
            sum = sum.wrapping_add(*b as u32);
        }
        Ok(sum)
    }
}

/// Statistics for the persistent buffer.
#[derive(Clone, Debug, Default)]
pub struct PersistentStats {
    /// Total items written
    pub total_written: u64,
    /// Total items read
    pub total_read: u64,
    /// Total sync operations
    pub sync_count: u64,
    /// Total bytes written
    pub bytes_written: u64,
    /// Items recovered on startup
    pub items_recovered: u64,
    /// Last sync timestamp (if any)
    pub last_sync: Option<Instant>,
}

/// A persistent circular buffer that survives process restarts.
///
/// This buffer uses memory-mapped files for efficient I/O and provides
/// configurable durability guarantees through different sync modes.
///
/// # Example
///
/// ```ignore
/// use elasticq::persistent::{PersistentCircularBuffer, PersistentConfig, SyncMode};
/// use std::time::Duration;
///
/// // Create a persistent buffer
/// let config = PersistentConfig::new("/tmp/my_queue.dat")
///     .with_sync_mode(SyncMode::Periodic(Duration::from_secs(1)));
///
/// let buffer = PersistentCircularBuffer::<String>::new(config)?;
///
/// // Push data (persisted to disk)
/// buffer.push("hello".to_string())?;
/// buffer.push("world".to_string())?;
///
/// // Explicitly sync if needed
/// buffer.sync()?;
///
/// // Data will be recovered on next startup
/// ```
pub struct PersistentCircularBuffer<T> {
    /// In-memory buffer for fast access
    buffer: Mutex<Vec<LogEntry<T>>>,
    /// Memory-mapped file for persistence (reserved for future optimization)
    #[allow(dead_code)]
    _mmap: RwLock<Option<MmapMut>>,
    /// File handle
    file: Mutex<File>,
    /// Configuration
    config: PersistentConfig,
    /// Current sequence number
    sequence: AtomicU64,
    /// Statistics
    stats: Mutex<PersistentStats>,
    /// Last sync time
    last_sync: Mutex<Instant>,
    /// Phantom data for type parameter
    _phantom: PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static> PersistentCircularBuffer<T> {
    /// Create a new persistent buffer.
    pub fn new(config: PersistentConfig) -> BufferResult<Self> {
        config.validate()?;

        // Create or open the data file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&config.data_path)
            .map_err(|e| BufferError::Other(format!("Failed to open file: {}", e)))?;

        // Get file size
        let file_size = file
            .metadata()
            .map_err(|e| BufferError::Other(format!("Failed to get file metadata: {}", e)))?
            .len() as usize;

        let mut buffer = Self {
            buffer: Mutex::new(Vec::new()),
            _mmap: RwLock::new(None),
            file: Mutex::new(file),
            config: config.clone(),
            sequence: AtomicU64::new(0),
            stats: Mutex::new(PersistentStats::default()),
            last_sync: Mutex::new(Instant::now()),
            _phantom: PhantomData,
        };

        // Recover data if file exists and is not empty
        if file_size > 0 && config.recover_on_startup {
            buffer.recover()?;
        } else {
            // Initialize new file with header
            buffer.initialize_file()?;
        }

        Ok(buffer)
    }

    /// Initialize a new data file with header.
    fn initialize_file(&self) -> BufferResult<()> {
        let mut file = self.file.lock();

        // Write initial header
        let header = FileHeader::new().with_checksum();
        let header_bytes = bincode::serialize(&header)
            .map_err(|e| BufferError::Other(format!("Failed to serialize header: {}", e)))?;

        // Create a fixed-size header buffer
        let mut header_buffer = vec![0u8; FileHeader::SIZE];
        let copy_len = header_bytes.len().min(FileHeader::SIZE);
        header_buffer[..copy_len].copy_from_slice(&header_bytes[..copy_len]);

        // Seek to beginning and write padded header
        file.seek(SeekFrom::Start(0))
            .map_err(|e| BufferError::Other(format!("Failed to seek: {}", e)))?;

        file.set_len(FileHeader::SIZE as u64)
            .map_err(|e| BufferError::Other(format!("Failed to set file length: {}", e)))?;

        file.write_all(&header_buffer)
            .map_err(|e| BufferError::Other(format!("Failed to write header: {}", e)))?;

        file.sync_all()
            .map_err(|e| BufferError::Other(format!("Failed to sync file: {}", e)))?;

        Ok(())
    }

    /// Recover data from the persistent file.
    fn recover(&mut self) -> BufferResult<()> {
        let file = self.file.lock();

        // Memory map the file
        let mmap = unsafe {
            MmapOptions::new()
                .map(&*file)
                .map_err(|e| BufferError::Other(format!("Failed to mmap file: {}", e)))?
        };

        if mmap.len() < FileHeader::SIZE {
            return Err(BufferError::Other("File too small for header".to_string()));
        }

        // Read and verify header
        let header: FileHeader = bincode::deserialize(&mmap[..FileHeader::SIZE])
            .map_err(|e| BufferError::Other(format!("Failed to deserialize header: {}", e)))?;

        if !header.verify_magic() {
            return Err(BufferError::Other("Invalid file magic".to_string()));
        }

        if !header.verify_checksum() {
            return Err(BufferError::Other("Header checksum mismatch".to_string()));
        }

        // Read entries from the file
        let mut entries = Vec::new();
        let mut offset = FileHeader::SIZE;
        let mut max_sequence = 0u64;

        while offset < mmap.len() {
            // Try to read entry length (4 bytes)
            if offset + 4 > mmap.len() {
                break;
            }

            let entry_len = u32::from_le_bytes([
                mmap[offset],
                mmap[offset + 1],
                mmap[offset + 2],
                mmap[offset + 3],
            ]) as usize;

            if entry_len == 0 || offset + 4 + entry_len > mmap.len() {
                break;
            }

            // Deserialize entry
            match bincode::deserialize::<LogEntry<T>>(&mmap[offset + 4..offset + 4 + entry_len]) {
                Ok(entry) => {
                    if entry.sequence > max_sequence {
                        max_sequence = entry.sequence;
                    }
                    entries.push(entry);
                }
                Err(_) => break, // Stop on first invalid entry
            }

            offset += 4 + entry_len;
        }

        drop(mmap);
        drop(file);

        // Update state
        self.sequence.store(max_sequence + 1, Ordering::SeqCst);
        let recovered_count = entries.len() as u64;
        *self.buffer.lock() = entries;

        // Update stats
        let mut stats = self.stats.lock();
        stats.items_recovered = recovered_count;

        Ok(())
    }

    /// Push an item to the buffer.
    pub fn push(&self, item: T) -> BufferResult<()> {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let entry = LogEntry::new(sequence, item)?;

        // Serialize entry
        let entry_bytes = bincode::serialize(&entry)
            .map_err(|e| BufferError::Other(format!("Serialization error: {}", e)))?;

        // Check size limit
        let entry_size = 4 + entry_bytes.len(); // 4 bytes for length prefix

        {
            let mut buffer = self.buffer.lock();
            let mut file = self.file.lock();

            // Check capacity
            if buffer.len() >= self.config.base.max_capacity {
                return Err(BufferError::MaxCapacityReached(self.config.base.max_capacity));
            }

            // Seek to end of file
            let current_pos = file
                .seek(SeekFrom::End(0))
                .map_err(|e| BufferError::Other(format!("Failed to seek to end: {}", e)))?;

            if current_pos + entry_size as u64 > self.config.max_file_size as u64 {
                return Err(BufferError::Other("Max file size exceeded".to_string()));
            }

            // Write length prefix
            file.write_all(&(entry_bytes.len() as u32).to_le_bytes())
                .map_err(|e| BufferError::Other(format!("Failed to write entry length: {}", e)))?;

            // Write entry
            file.write_all(&entry_bytes)
                .map_err(|e| BufferError::Other(format!("Failed to write entry: {}", e)))?;

            // Add to in-memory buffer
            buffer.push(entry);

            // Update stats
            let mut stats = self.stats.lock();
            stats.total_written += 1;
            stats.bytes_written += entry_size as u64;
        }

        // Handle sync mode
        self.maybe_sync()?;

        Ok(())
    }

    /// Pop an item from the buffer.
    pub fn pop(&self) -> BufferResult<T> {
        let mut buffer = self.buffer.lock();

        if buffer.is_empty() {
            return Err(BufferError::Empty);
        }

        let entry = buffer.remove(0);

        // Update stats
        let mut stats = self.stats.lock();
        stats.total_read += 1;

        Ok(entry.data)
    }

    /// Check the sync mode and sync if necessary.
    fn maybe_sync(&self) -> BufferResult<()> {
        match self.config.sync_mode {
            SyncMode::NoSync => Ok(()),
            SyncMode::EveryWrite => self.sync(),
            SyncMode::Periodic(interval) => {
                let last_sync = *self.last_sync.lock();
                if last_sync.elapsed() >= interval {
                    self.sync()
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Explicitly sync the buffer to disk.
    pub fn sync(&self) -> BufferResult<()> {
        let file = self.file.lock();
        file.sync_all()
            .map_err(|e| BufferError::Other(format!("Failed to sync: {}", e)))?;

        *self.last_sync.lock() = Instant::now();

        let mut stats = self.stats.lock();
        stats.sync_count += 1;
        stats.last_sync = Some(Instant::now());

        Ok(())
    }

    /// Get the number of items in the buffer.
    pub fn len(&self) -> usize {
        self.buffer.lock().len()
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.lock().is_empty()
    }

    /// Get statistics about the buffer.
    pub fn stats(&self) -> PersistentStats {
        self.stats.lock().clone()
    }

    /// Clear the buffer and reset the file.
    pub fn clear(&self) -> BufferResult<()> {
        self.buffer.lock().clear();
        self.sequence.store(0, Ordering::SeqCst);

        // Truncate and reinitialize file
        {
            let file = self.file.lock();
            file.set_len(0)
                .map_err(|e| BufferError::Other(format!("Failed to truncate file: {}", e)))?;
        }

        self.initialize_file()?;
        Ok(())
    }

    /// Compact the file by removing already-consumed entries.
    ///
    /// This is useful for reclaiming disk space after many pop operations.
    pub fn compact(&self) -> BufferResult<()> {
        let buffer = self.buffer.lock();
        let entries: Vec<_> = buffer.clone();
        drop(buffer);

        // Create a new file
        let temp_path = self.config.data_path.with_extension("tmp");
        let mut temp_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(|e| BufferError::Other(format!("Failed to create temp file: {}", e)))?;

        // Write header
        let header = FileHeader::new().with_checksum();
        let header_bytes = bincode::serialize(&header)
            .map_err(|e| BufferError::Other(format!("Failed to serialize header: {}", e)))?;

        temp_file
            .write_all(&header_bytes)
            .map_err(|e| BufferError::Other(format!("Failed to write header: {}", e)))?;

        // Pad header to fixed size
        let padding = vec![0u8; FileHeader::SIZE - header_bytes.len()];
        temp_file
            .write_all(&padding)
            .map_err(|e| BufferError::Other(format!("Failed to write padding: {}", e)))?;

        // Write all entries
        for entry in &entries {
            let entry_bytes = bincode::serialize(entry)
                .map_err(|e| BufferError::Other(format!("Serialization error: {}", e)))?;

            temp_file
                .write_all(&(entry_bytes.len() as u32).to_le_bytes())
                .map_err(|e| BufferError::Other(format!("Failed to write entry length: {}", e)))?;

            temp_file
                .write_all(&entry_bytes)
                .map_err(|e| BufferError::Other(format!("Failed to write entry: {}", e)))?;
        }

        temp_file
            .sync_all()
            .map_err(|e| BufferError::Other(format!("Failed to sync temp file: {}", e)))?;

        drop(temp_file);

        // Replace old file with new file
        std::fs::rename(&temp_path, &self.config.data_path)
            .map_err(|e| BufferError::Other(format!("Failed to rename file: {}", e)))?;

        // Reopen file
        let new_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.config.data_path)
            .map_err(|e| BufferError::Other(format!("Failed to reopen file: {}", e)))?;

        *self.file.lock() = new_file;

        Ok(())
    }

    /// Push a batch of items.
    pub fn push_batch(&self, items: Vec<T>) -> BufferResult<()> {
        for item in items {
            self.push(item)?;
        }
        Ok(())
    }

    /// Pop a batch of items.
    pub fn pop_batch(&self, max_items: usize) -> BufferResult<Vec<T>> {
        let mut result = Vec::with_capacity(max_items);

        for _ in 0..max_items {
            match self.pop() {
                Ok(item) => result.push(item),
                Err(BufferError::Empty) => break,
                Err(e) => return Err(e),
            }
        }

        Ok(result)
    }
}

impl<T> Drop for PersistentCircularBuffer<T> {
    fn drop(&mut self) {
        // Try to sync on drop
        if let Some(file) = self.file.try_lock() {
            let _ = file.sync_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_basic_push_pop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dat");

        let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::NoSync);
        let buffer = PersistentCircularBuffer::<String>::new(config).unwrap();

        buffer.push("hello".to_string()).unwrap();
        buffer.push("world".to_string()).unwrap();

        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.pop().unwrap(), "hello");
        assert_eq!(buffer.pop().unwrap(), "world");
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_persistence_and_recovery() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dat");

        // Create and populate buffer
        {
            let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::EveryWrite);
            let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();

            buffer.push(1).unwrap();
            buffer.push(2).unwrap();
            buffer.push(3).unwrap();

            buffer.sync().unwrap();
        }

        // Recover from file
        {
            let config = PersistentConfig::new(&path);
            let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();

            let stats = buffer.stats();
            assert_eq!(stats.items_recovered, 3);

            assert_eq!(buffer.pop().unwrap(), 1);
            assert_eq!(buffer.pop().unwrap(), 2);
            assert_eq!(buffer.pop().unwrap(), 3);
        }
    }

    #[test]
    fn test_clear() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dat");

        let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::NoSync);
        let buffer = PersistentCircularBuffer::<String>::new(config).unwrap();

        buffer.push("test".to_string()).unwrap();
        assert!(!buffer.is_empty());

        buffer.clear().unwrap();
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_batch_operations() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dat");

        let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::NoSync);
        let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();

        buffer.push_batch(vec![1, 2, 3, 4, 5]).unwrap();
        assert_eq!(buffer.len(), 5);

        let items = buffer.pop_batch(3).unwrap();
        assert_eq!(items, vec![1, 2, 3]);
        assert_eq!(buffer.len(), 2);
    }

    #[test]
    fn test_compact() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dat");

        let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::NoSync);
        let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();

        // Push many items
        for i in 0..100 {
            buffer.push(i).unwrap();
        }

        // Pop most of them
        for _ in 0..90 {
            buffer.pop().unwrap();
        }

        // Compact
        buffer.compact().unwrap();

        // Verify remaining items
        assert_eq!(buffer.len(), 10);
        for i in 90..100 {
            assert_eq!(buffer.pop().unwrap(), i);
        }
    }

    #[test]
    fn test_stats() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dat");

        let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::EveryWrite);
        let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();

        buffer.push(1).unwrap();
        buffer.push(2).unwrap();
        buffer.pop().unwrap();

        let stats = buffer.stats();
        assert_eq!(stats.total_written, 2);
        assert_eq!(stats.total_read, 1);
        assert!(stats.sync_count >= 2); // At least one sync per write
    }

    #[test]
    fn test_empty_pop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dat");

        let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::NoSync);
        let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();

        assert!(matches!(buffer.pop(), Err(BufferError::Empty)));
    }

    #[test]
    fn test_sync_modes() {
        let dir = tempdir().unwrap();

        // Test NoSync
        {
            let path = dir.path().join("nosync.dat");
            let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::NoSync);
            let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();
            buffer.push(1).unwrap();
            let stats = buffer.stats();
            assert_eq!(stats.sync_count, 0);
        }

        // Test EveryWrite
        {
            let path = dir.path().join("everywrite.dat");
            let config = PersistentConfig::new(&path).with_sync_mode(SyncMode::EveryWrite);
            let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();
            buffer.push(1).unwrap();
            buffer.push(2).unwrap();
            let stats = buffer.stats();
            assert_eq!(stats.sync_count, 2);
        }
    }
}
