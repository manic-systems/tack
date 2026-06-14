// SPDX-License-Identifier: EUPL-1.2

/// expected user-facing failure without an eyre report
#[derive(thiserror::Error, Debug)]
#[error("{0}")]
pub struct UserError(pub String);

macro_rules! user_bail {
    ($($arg:tt)*) => {
        return ::core::result::Result::Err(
            ::eyre::Report::new($crate::error::UserError(::std::format!($($arg)*)))
        )
    };
}

pub(crate) use user_bail;
