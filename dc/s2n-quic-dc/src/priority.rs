#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Priority {
    Highest = 0,
    VeryHigh = 1,
    High = 2,
    Medium = 3,
    MediumLow = 4,
    Low = 5,
    VeryLow = 6,
    Lowest = 7,
}

impl Default for Priority {
    fn default() -> Self {
        Self::MEDIUM
    }
}

impl TryFrom<u8> for Priority {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Highest),
            1 => Ok(Self::VeryHigh),
            2 => Ok(Self::High),
            3 => Ok(Self::Medium),
            4 => Ok(Self::MediumLow),
            5 => Ok(Self::Low),
            6 => Ok(Self::VeryLow),
            7 => Ok(Self::Lowest),
            _ => Err(()),
        }
    }
}

impl Priority {
    pub const HIGHEST: Priority = Priority::Highest;
    pub const VERY_HIGH: Priority = Priority::VeryHigh;
    pub const HIGH: Priority = Priority::High;
    pub const MEDIUM: Priority = Priority::Medium;
    pub const MEDIUM_LOW: Priority = Priority::MediumLow;
    pub const LOW: Priority = Priority::Low;
    pub const VERY_LOW: Priority = Priority::VeryLow;
    pub const LOWEST: Priority = Priority::Lowest;

    pub const fn from_u4(value: u8) -> Self {
        match value & 0b111 {
            0 => Self::Highest,
            1 => Self::VeryHigh,
            2 => Self::High,
            3 => Self::Medium,
            4 => Self::MediumLow,
            5 => Self::Low,
            6 => Self::VeryLow,
            7 => Self::Lowest,
            _ => unreachable!(),
        }
    }
}
