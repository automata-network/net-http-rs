use core::time::Duration;
use std::prelude::v1::*;

use std::sync::{Arc, Mutex};

use base::time::Time;

#[derive(Default, Debug)]
pub struct ConnPool<T> {
    idle_time: Option<Duration>,
    list: Arc<Mutex<Vec<(T, Time)>>>,
}

impl<T> Clone for ConnPool<T> {
    fn clone(&self) -> Self {
        Self {
            idle_time: self.idle_time.clone(),
            list: self.list.clone(),
        }
    }
}

impl<T> ConnPool<T> {
    pub fn new(idle_time: Option<Duration>) -> Self {
        Self {
            idle_time,
            list: Default::default(),
        }
    }

    pub fn get_or<F, E>(&self, f: F) -> Result<ConnPoolGuard<T>, E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        match self.pop() {
            Some(n) => Ok(n),
            None => {
                let item = f()?;
                Ok(ConnPoolGuard::new(self.list.clone(), item, false))
            }
        }
    }

    pub fn pop(&self) -> Option<ConnPoolGuard<T>> {
        let mut list = self.list.lock().unwrap();
        loop {
            let (item, create_time) = list.pop()?; 
            if let Some(timeout) = &self.idle_time {
                if Time::now() - create_time >= *timeout {
                    continue;
                }
            }
            return Some(ConnPoolGuard::new(self.list.clone(), item, true));
        }
    }
}

pub struct ConnPoolGuard<T> {
    create_time: Time,
    reused: bool,
    list: Arc<Mutex<Vec<(T, Time)>>>,
    raw: Option<T>,
    drop: bool,
}

impl<T> std::ops::Deref for ConnPoolGuard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.raw.as_ref().unwrap()
    }
}

impl<T> std::ops::DerefMut for ConnPoolGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.raw.as_mut().unwrap()
    }
}

impl<T> ConnPoolGuard<T> {
    pub fn new(list: Arc<Mutex<Vec<(T, Time)>>>, item: T, reused: bool) -> Self {
        Self {
            list,
            create_time: Time::now(),
            raw: Some(item),
            drop: true,
            reused,
        }
    }

    pub fn reuse(mut self) {
        self.drop = false;
    }

    pub fn is_reused(&self) -> bool {
        self.reused
    }
}

impl<T> Drop for ConnPoolGuard<T> {
    fn drop(&mut self) {
        if !self.drop {
            match self.raw.take() {
                Some(item) => self.list.lock().unwrap().push((item, self.create_time)),
                None => {}
            }
        }
    }
}
