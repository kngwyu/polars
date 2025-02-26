use std::fmt::Write;

use arrow::temporal_conversions::{
    timestamp_ms_to_datetime, timestamp_ns_to_datetime, timestamp_us_to_datetime,
};
#[cfg(feature = "timezones")]
use polars_arrow::kernels::cast_timezone;

use super::conversion::{datetime_to_timestamp_ms, datetime_to_timestamp_ns};
use super::*;
use crate::prelude::DataType::Datetime;
use crate::prelude::*;

#[cfg(feature = "timezones")]
fn validate_time_zone(tz: TimeZone) -> PolarsResult<()> {
    use arrow::temporal_conversions::parse_offset;
    use chrono_tz::Tz;
    match parse_offset(&tz) {
        Ok(_) => Ok(()),
        Err(_) => match tz.parse::<Tz>() {
            Ok(_) => Ok(()),
            Err(_) => Err(PolarsError::ComputeError(
                format!("Could not parse timezone: '{tz}'").into(),
            )),
        },
    }
}

impl DatetimeChunked {
    pub fn as_datetime_iter(
        &self,
    ) -> impl Iterator<Item = Option<NaiveDateTime>> + TrustedLen + '_ {
        let func = match self.time_unit() {
            TimeUnit::Nanoseconds => timestamp_ns_to_datetime,
            TimeUnit::Microseconds => timestamp_us_to_datetime,
            TimeUnit::Milliseconds => timestamp_ms_to_datetime,
        };
        // we know the iterators len
        unsafe {
            self.downcast_iter()
                .flat_map(move |iter| iter.into_iter().map(move |opt_v| opt_v.copied().map(func)))
                .trust_my_length(self.len())
        }
    }

    pub fn time_unit(&self) -> TimeUnit {
        match self.2.as_ref().unwrap() {
            DataType::Datetime(tu, _) => *tu,
            _ => unreachable!(),
        }
    }

    pub fn time_zone(&self) -> &Option<TimeZone> {
        match self.2.as_ref().unwrap() {
            DataType::Datetime(_, tz) => tz,
            _ => unreachable!(),
        }
    }

    pub fn apply_on_tz_corrected<F>(&self, mut func: F) -> PolarsResult<DatetimeChunked>
    where
        F: FnMut(DatetimeChunked) -> PolarsResult<DatetimeChunked>,
    {
        #[allow(unused_mut)]
        let mut ca = self.clone();
        #[cfg(feature = "timezones")]
        if self.time_zone().is_some() {
            ca = self.cast_time_zone(Some("UTC"))?
        }
        let out = func(ca)?;

        #[cfg(feature = "timezones")]
        if let Some(tz) = self.time_zone() {
            return out
                .with_time_zone("UTC".to_string())?
                .cast_time_zone(Some(tz));
        }
        Ok(out)
    }

    #[cfg(feature = "timezones")]
    pub fn cast_time_zone(&self, tz: Option<&str>) -> PolarsResult<DatetimeChunked> {
        match (self.time_zone(), tz) {
            (Some(from), Some(to)) => {
                let chunks = self
                    .downcast_iter()
                    .map(|arr| {
                        Ok(cast_timezone(
                            arr,
                            self.time_unit().to_arrow(),
                            to.to_string(),
                            from.to_string(),
                        )?)
                    })
                    .collect::<PolarsResult<_>>()?;
                let out = unsafe { ChunkedArray::from_chunks(self.name(), chunks) };
                Ok(out.into_datetime(self.time_unit(), Some(to.to_string())))
            }
            (Some(from), None) => {
                let chunks = self
                    .downcast_iter()
                    .map(|arr| {
                        Ok(cast_timezone(
                            arr,
                            self.time_unit().to_arrow(),
                            "UTC".to_string(),
                            from.to_string(),
                        )?)
                    })
                    .collect::<PolarsResult<_>>()?;
                let out = unsafe { ChunkedArray::from_chunks(self.name(), chunks) };
                Ok(out.into_datetime(self.time_unit(), None))
            }
            (None, Some(to)) => {
                let chunks = self
                    .downcast_iter()
                    .map(|arr| {
                        Ok(cast_timezone(
                            arr,
                            self.time_unit().to_arrow(),
                            to.to_string(),
                            "UTC".to_string(),
                        )?)
                    })
                    .collect::<PolarsResult<_>>()?;
                let out = unsafe { ChunkedArray::from_chunks(self.name(), chunks) };
                Ok(out.into_datetime(self.time_unit(), Some(to.to_string())))
            }
            (None, None) => Ok(self.clone()),
        }
    }

    /// Format Datetime with a `fmt` rule. See [chrono strftime/strptime](https://docs.rs/chrono/0.4.19/chrono/format/strftime/index.html).
    pub fn strftime(&self, fmt: &str) -> Utf8Chunked {
        let conversion_f = match self.time_unit() {
            TimeUnit::Nanoseconds => timestamp_ns_to_datetime,
            TimeUnit::Microseconds => timestamp_us_to_datetime,
            TimeUnit::Milliseconds => timestamp_ms_to_datetime,
        };

        let dt = NaiveDate::from_ymd_opt(2001, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let fmted = format!("{}", dt.format(fmt));

        #[allow(unused_mut)]
        let mut ca = self.clone();
        #[cfg(feature = "timezones")]
        if self.time_zone().is_some() {
            ca = ca.cast_time_zone(Some("UTC")).unwrap();
        }

        let mut ca: Utf8Chunked = ca.apply_kernel_cast(&|arr| {
            let mut buf = String::new();
            let mut mutarr =
                MutableUtf8Array::with_capacities(arr.len(), arr.len() * fmted.len() + 1);

            for opt in arr.into_iter() {
                match opt {
                    None => mutarr.push_null(),
                    Some(v) => {
                        buf.clear();
                        let datefmt = conversion_f(*v).format(fmt);
                        write!(buf, "{datefmt}").unwrap();
                        mutarr.push(Some(&buf))
                    }
                }
            }

            let arr: Utf8Array<i64> = mutarr.into();
            Box::new(arr)
        });
        ca.rename(self.name());
        ca
    }

    /// Construct a new [`DatetimeChunked`] from an iterator over [`NaiveDateTime`].
    pub fn from_naive_datetime<I: IntoIterator<Item = NaiveDateTime>>(
        name: &str,
        v: I,
        tu: TimeUnit,
    ) -> Self {
        let func = match tu {
            TimeUnit::Nanoseconds => datetime_to_timestamp_ns,
            TimeUnit::Microseconds => datetime_to_timestamp_us,
            TimeUnit::Milliseconds => datetime_to_timestamp_ms,
        };
        let vals = v.into_iter().map(func).collect::<Vec<_>>();
        Int64Chunked::from_vec(name, vals).into_datetime(tu, None)
    }

    pub fn from_naive_datetime_options<I: IntoIterator<Item = Option<NaiveDateTime>>>(
        name: &str,
        v: I,
        tu: TimeUnit,
    ) -> Self {
        let func = match tu {
            TimeUnit::Nanoseconds => datetime_to_timestamp_ns,
            TimeUnit::Microseconds => datetime_to_timestamp_us,
            TimeUnit::Milliseconds => datetime_to_timestamp_ms,
        };
        let vals = v.into_iter().map(|opt_nd| opt_nd.map(func));
        Int64Chunked::from_iter_options(name, vals).into_datetime(tu, None)
    }

    /// Change the underlying [`TimeUnit`]. And update the data accordingly.
    #[must_use]
    pub fn cast_time_unit(&self, tu: TimeUnit) -> Self {
        let current_unit = self.time_unit();
        let mut out = self.clone();
        out.set_time_unit(tu);

        use TimeUnit::*;
        match (current_unit, tu) {
            (Nanoseconds, Microseconds) => {
                let ca = &self.0 / 1_000;
                out.0 = ca;
                out
            }
            (Nanoseconds, Milliseconds) => {
                let ca = &self.0 / 1_000_000;
                out.0 = ca;
                out
            }
            (Microseconds, Nanoseconds) => {
                let ca = &self.0 * 1_000;
                out.0 = ca;
                out
            }
            (Microseconds, Milliseconds) => {
                let ca = &self.0 / 1_000;
                out.0 = ca;
                out
            }
            (Milliseconds, Nanoseconds) => {
                let ca = &self.0 * 1_000_000;
                out.0 = ca;
                out
            }
            (Milliseconds, Microseconds) => {
                let ca = &self.0 * 1_000;
                out.0 = ca;
                out
            }
            (Nanoseconds, Nanoseconds)
            | (Microseconds, Microseconds)
            | (Milliseconds, Milliseconds) => out,
        }
    }

    /// Change the underlying [`TimeUnit`]. This does not modify the data.
    pub fn set_time_unit(&mut self, tu: TimeUnit) {
        self.2 = Some(Datetime(tu, self.time_zone().clone()))
    }

    /// Change the underlying [`TimeZone`]. This does not modify the data.
    #[cfg(feature = "timezones")]
    pub fn set_time_zone(&mut self, tz: TimeZone) -> PolarsResult<()> {
        validate_time_zone(tz.to_string())?;
        self.2 = Some(Datetime(self.time_unit(), Some(tz)));
        Ok(())
    }
    #[cfg(feature = "timezones")]
    pub fn with_time_zone(mut self, tz: TimeZone) -> PolarsResult<Self> {
        match self.time_zone() {
            Some(_) => {
                self.set_time_zone(tz)?;
                Ok(self)
            }
            _ => Err(PolarsError::ComputeError(
                "Cannot call with_time_zone on tz-naive. Set a time zone first with cast_time_zone"
                    .into(),
            )),
        }
    }
}

#[cfg(test)]
mod test {
    use chrono::NaiveDateTime;

    use crate::prelude::*;

    #[test]
    fn from_datetime() {
        let datetimes: Vec<_> = [
            "1988-08-25 00:00:16",
            "2015-09-05 23:56:04",
            "2012-12-21 00:00:00",
        ]
        .iter()
        .map(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap())
        .collect();

        // NOTE: the values are checked and correct.
        let dt = DatetimeChunked::from_naive_datetime(
            "name",
            datetimes.iter().copied(),
            TimeUnit::Nanoseconds,
        );
        assert_eq!(
            [
                588470416000_000_000,
                1441497364000_000_000,
                1356048000000_000_000
            ],
            dt.cont_slice().unwrap()
        );
    }
}
