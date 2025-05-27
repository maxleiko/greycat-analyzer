use std::mem::MaybeUninit;

/// A bound stack
#[derive(Debug, Clone)]
pub struct LexerStack<T: Copy, const N: usize> {
    // Uninitialized array storage
    data: [MaybeUninit<T>; N],
    // Current length/top index
    len: usize,
}

impl<T: Copy, const N: usize> LexerStack<T, N> {
    pub fn new() -> Self {
        Self {
            // SAFETY:
            // This is safe because MaybeUninit<T> doesn't need initialization
            // and we always bound check `.data` access with `.len`
            data: unsafe { MaybeUninit::uninit().assume_init() },
            len: 0,
        }
    }

    pub fn push(&mut self, value: T) -> Result<(), T> {
        if self.len >= N {
            return Err(value);
        }

        // Write to uninitialized memory
        self.data[self.len].write(value);
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }

        self.len -= 1;
        // Read from initialized memory
        Some(unsafe { self.data[self.len].assume_init() })
    }
}
