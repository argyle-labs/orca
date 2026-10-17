use std::fmt;

#[derive(Debug)]
pub struct PeerRefusal(pub String);

impl fmt::Display for PeerRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PeerRefusal {}

macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err(anyhow::Error::new($crate::mesh::refusal::PeerRefusal(format!($($arg)*))))
    };
}

macro_rules! refuse_unless {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            $crate::mesh::refusal::refuse!($($arg)*);
        }
    };
}

pub(crate) use refuse;
pub(crate) use refuse_unless;
