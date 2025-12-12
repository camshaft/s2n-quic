macro_rules! define_enum {
    (pub enum $name:ident {
        $(
            $(#[doc = $doc:literal])*
            $variant:ident = $value:ident
        ),* $(,)?
    }) => {
        #[derive(Debug, PartialEq, Eq, Clone, Copy)]
        #[repr(u32)]
        #[allow(non_camel_case_types)]
        pub enum $name {
            $($variant = $value),*
        }

        impl $name {
            #[allow(non_upper_case_globals)]
            pub fn from_bits(bits: u32) -> Option<Self> {
                match bits {
                    $($value => Some(Self::$variant)),*,
                    _ => None
                }
            }
        }
    }
}
