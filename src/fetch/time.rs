// SPDX-License-Identifier: EUPL-1.2

use std::ops::Range;

use eyre::{
    ContextCompat as _,
    Result,
    WrapErr as _,
    bail,
};

pub(super) fn epoch_from_http_date(input: &str) -> Result<i64> {
    let bytes = input.as_bytes();
    if bytes.len() < 29 {
        bail!("bad http date: {input}");
    }
    let slice = |range: Range<usize>| -> Result<&str> {
        input
            .get(range)
            .wrap_err_with(|| format!("bad http date: {input}"))
    };
    let parse_num = |range: Range<usize>| -> Result<i64> {
        slice(range)?
            .parse()
            .wrap_err_with(|| format!("bad http date: {input}"))
    };
    let day = parse_num(5..7)?;
    let month = match slice(8..11)? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        name => bail!("bad month in http date: {name}"),
    };
    let year = parse_num(12..16)?;
    let hh = parse_num(17..19)?;
    let mi = parse_num(20..22)?;
    let ss = parse_num(23..25)?;
    Ok(days_from_civil(year, month, day) * 86400 + hh * 3600 + mi * 60 + ss)
}

pub(super) fn epoch_from_iso(input: &str) -> Result<i64> {
    let bytes = input.as_bytes();
    if bytes.len() < 20 {
        bail!("bad timestamp: {input}");
    }
    let parse_num = |range: Range<usize>| -> Result<i64> {
        input
            .get(range)
            .wrap_err_with(|| format!("bad timestamp: {input}"))?
            .parse()
            .wrap_err_with(|| format!("bad timestamp: {input}"))
    };
    let (year, month, day) = (parse_num(0..4)?, parse_num(5..7)?, parse_num(8..10)?);
    let (hh, mi, ss) = (parse_num(11..13)?, parse_num(14..16)?, parse_num(17..19)?);
    Ok(days_from_civil(year, month, day) * 86400 + hh * 3600 + mi * 60 + ss)
}

const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let yoe = adjusted_year - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
