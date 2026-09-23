// SPDX-License-Identifier: EUPL-1.2

/// expected user-facing failure without a full report
#[derive(thiserror::Error, Debug)]
#[error("{0}")]
pub struct UserError(pub String);

macro_rules! user_bail {
    ($($arg:tt)*) => {
        return ::core::result::Result::Err(
            ::misstep::Report::new($crate::error::UserError(::std::format!($($arg)*)))
        )
    };
}

pub(crate) use user_bail;
