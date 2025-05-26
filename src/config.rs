use crate::BufferError;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub initial_capacity: usize,
    pub min_capacity: usize,
    pub max_capacity: usize,
    pub growth_factor: f64,
    pub shrink_threshold: f64,
    pub push_timeout: Option<Duration>,
    pub pop_timeout: Option<Duration>,
}

impl Config {
    pub fn validate(&self) -> Result<(), BufferError> {
        if self.min_capacity == 0 || self.max_capacity == 0 {
            return Err(BufferError::InvalidConfiguration(
                "Capacities must be greater than zero".to_string(),
            ));
        }
        if self.min_capacity > self.max_capacity {
            return Err(BufferError::InvalidConfiguration(
                "min_capacity cannot be greater than max_capacity".to_string(),
            ));
        }
        if self.initial_capacity < self.min_capacity || self.initial_capacity > self.max_capacity {
            return Err(BufferError::InvalidConfiguration(
                "initial_capacity must be between min_capacity and max_capacity".to_string(),
            ));
        }
        if self.growth_factor <= 1.0 {
            return Err(BufferError::InvalidConfiguration(
                "growth_factor must be greater than 1.0".to_string(),
            ));
        }
        if !(0.0 < self.shrink_threshold && self.shrink_threshold < 1.0) {
            return Err(BufferError::InvalidConfiguration(
                "shrink_threshold must be between 0 and 1".to_string(),
            ));
        }
        Ok(())
    }

    pub fn with_initial_capacity(mut self, capacity: usize) -> Self {
        self.initial_capacity = capacity;
        self
    }

    pub fn with_min_capacity(mut self, capacity: usize) -> Self {
        self.min_capacity = capacity;
        self
    }

    pub fn with_max_capacity(mut self, capacity: usize) -> Self {
        self.max_capacity = capacity;
        self
    }

    pub fn with_growth_factor(mut self, factor: f64) -> Self {
        self.growth_factor = factor;
        self
    }

    pub fn with_shrink_threshold(mut self, threshold: f64) -> Self {
        self.shrink_threshold = threshold;
        self
    }

    pub fn with_push_timeout(mut self, timeout: Duration) -> Self {
        self.push_timeout = Some(timeout);
        self
    }

    pub fn with_pop_timeout(mut self, timeout: Duration) -> Self {
        self.pop_timeout = Some(timeout);
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            initial_capacity: 1024,
            min_capacity: 1024,
            max_capacity: 65536,
            growth_factor: 2.0,
            shrink_threshold: 0.25,
            push_timeout: None,
            pop_timeout: None,
        }
    }
}
