// SPDX-License-Identifier: EUPL-1.2

/// an expected, user-facing failure: bad usage, a missing or duplicate pin,
/// drift. `run` renders these as a plain `tack: <message>` line, reserving the
/// full eyre report for genuine bugs
#[derive(thiserror::Error, Debug)]
#[error("{0}")]
pub struct UserError(pub String);

/// `bail!` for expected user errors: returns a `UserError` through eyre
macro_rules! user_bail {
    ($($arg:tt)*) => {
        return ::core::result::Result::Err(
            ::eyre::Report::new($crate::error::UserError(::std::format!($($arg)*)))
        )
    };
}

pub(crate) use user_bail;
