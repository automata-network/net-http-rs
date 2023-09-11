use std::prelude::v1::*;

use std::sync::{Arc, Mutex};

#[derive(Default, Debug)]
pub struct ConnPool<T> {
    list: Arc<Mutex<Vec<T>>>,
}

impl<T> Clone for ConnPool<T> {
    fn clone(&self) -> Self {
        Self {
            list: self.list.clone(),
        }
    }
}

impl<T> ConnPool<T> {
    pub fn new() -> Self {
        Self {
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
        list.pop()
            .map(|item| ConnPoolGuard::new(self.list.clone(), item, true))
    }
}

pub struct ConnPoolGuard<T> {
    reused: bool,
    list: Arc<Mutex<Vec<T>>>,
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
    pub fn new(list: Arc<Mutex<Vec<T>>>, item: T, reused: bool) -> Self {
        Self {
            list,
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
                Some(item) => self.list.lock().unwrap().push(item),
                None => {}
            }
        }
    }
}
