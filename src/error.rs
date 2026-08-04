use std::{error::Error as StdError, fmt};

pub type Error = Box<dyn StdError + Send + Sync + 'static>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct MessageError(pub String);

impl fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for MessageError {}

pub trait Context<T> {
    fn context(self, message: impl fmt::Display) -> Result<T>;

    fn with_context(self, message: impl FnOnce() -> String) -> Result<T>
    where
        Self: Sized,
    {
        self.context(message())
    }
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: fmt::Display,
{
    fn context(self, message: impl fmt::Display) -> Result<T> {
        self.map_err(|error| MessageError(format!("{message}: {error}")).into())
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, message: impl fmt::Display) -> Result<T> {
        self.ok_or_else(|| MessageError(message.to_string()).into())
    }
}

#[macro_export]
macro_rules! bail {
    ($($argument:tt)*) => {
        return Err($crate::error::MessageError(format!($($argument)*)).into())
    };
}

#[macro_export]
macro_rules! anyhow {
    ($error:expr) => {
        Box::<dyn std::error::Error + Send + Sync>::from($error)
    };
    ($($argument:tt)*) => {
        Box::<dyn std::error::Error + Send + Sync>::from(
            $crate::error::MessageError(format!($($argument)*))
        )
    };
}
