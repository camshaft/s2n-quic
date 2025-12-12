#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Priority(u8);

impl Default for Priority {
    fn default() -> Self {
        Self::MEDIUM
    }
}

impl Priority {
    pub const HIGHEST: Priority = Priority(0);
    pub const VERY_HIGH: Priority = Priority(1);
    pub const HIGH: Priority = Priority(2);
    pub const MEDIUM: Priority = Priority(3);
    pub const MEDIUM_LOW: Priority = Priority(4);
    pub const LOW: Priority = Priority(5);
    pub const VERY_LOW: Priority = Priority(6);
    pub const LOWEST: Priority = Priority(7);
}
