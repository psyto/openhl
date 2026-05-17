use core::fmt;

use informalsystems_malachitebft_core_types::Address;

/// A 20-byte validator address, Ethereum convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpenHlAddress(pub [u8; 20]);

impl fmt::Display for OpenHlAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl Address for OpenHlAddress {}
