use chrono::{Duration, NaiveDateTime, Timelike};

/// 默认定时开始时间：当前本地时间 + 30 分钟，秒取整为 0。
///
/// 秒取整为 0 的原因：定时控件本身按「年月日时分」选择，默认值若带秒会在
/// 提示里出现 `12:47:42` 之类的零碎秒数，和用户手选时间的习惯不一致。
pub fn default_start(now: NaiveDateTime) -> NaiveDateTime {
    let base = now
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);
    base + Duration::minutes(30)
}

/// 把定时时间平移 delta 秒（可正可负），返回 None 表示超出可表示范围。
///
/// 用 chrono 的 `checked_add_signed` / `checked_sub_signed` 做真正的日期时间
/// 运算，跨天、跨月、跨年都由 chrono 处理，避免手写减法只改时分不改日期。
pub fn shift(base: NaiveDateTime, delta: Duration) -> Option<NaiveDateTime> {
    if delta.num_seconds() < 0 {
        base.checked_sub_signed(-delta)
    } else {
        base.checked_add_signed(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(hh, mm, ss)
            .unwrap()
    }

    #[test]
    fn default_is_now_plus_30_minutes() {
        let now = dt(2026, 10, 30, 12, 17, 42);
        let t = default_start(now);
        // 秒取整为 0：12:17:42 → 12:17:00 + 30min = 12:47:00
        assert_eq!(t, dt(2026, 10, 30, 12, 47, 0));
        assert_eq!(t.second(), 0);
        // 与 now 的差值落在 [29min, 30min] 区间内（秒取整带来的 <1 分钟误差）
        let diff = (t - now).num_seconds();
        assert!(
            (29 * 60..=30 * 60).contains(&diff),
            "diff={diff}s 不在 29~30 分钟区间"
        );
    }

    #[test]
    fn default_crosses_midnight() {
        let now = dt(2026, 10, 30, 23, 50, 30);
        let t = default_start(now);
        assert_eq!(t, dt(2026, 10, 31, 0, 20, 0));
        assert_eq!(t.date(), NaiveDate::from_ymd_opt(2026, 10, 31).unwrap());
    }

    #[test]
    fn minus_10_minutes_crosses_midnight() {
        // 23:55 的「昨天版」：00:05 − 10 分钟必须回到前一天 23:55
        let base = dt(2026, 10, 31, 0, 5, 0);
        let t = shift(base, Duration::minutes(-10)).unwrap();
        assert_eq!(t, dt(2026, 10, 30, 23, 55, 0));
        assert_eq!(t.date(), NaiveDate::from_ymd_opt(2026, 10, 30).unwrap());
    }

    #[test]
    fn minus_1_hour_crosses_day() {
        let base = dt(2026, 11, 1, 0, 30, 0);
        let t = shift(base, Duration::hours(-1)).unwrap();
        assert_eq!(t, dt(2026, 10, 31, 23, 30, 0));
    }

    #[test]
    fn minus_crosses_month() {
        // 3 月 1 日 00:10 − 30 分钟 → 2 月 28 日 23:40（2026 非闰年）
        let base = dt(2026, 3, 1, 0, 10, 0);
        let t = shift(base, Duration::minutes(-30)).unwrap();
        assert_eq!(t, dt(2026, 2, 28, 23, 40, 0));
    }

    #[test]
    fn minus_crosses_year() {
        let base = dt(2026, 1, 1, 0, 20, 0);
        let t = shift(base, Duration::hours(-1)).unwrap();
        assert_eq!(t, dt(2025, 12, 31, 23, 20, 0));
    }

    #[test]
    fn add_and_sub_are_inverse() {
        let cases = [
            dt(2026, 10, 30, 12, 0, 30),
            dt(2026, 10, 31, 0, 3, 0),
            dt(2026, 1, 1, 0, 10, 0),
            dt(2026, 3, 1, 0, 20, 0),
        ];
        for base in cases {
            for delta in [
                Duration::minutes(5),
                Duration::minutes(10),
                Duration::hours(1),
            ] {
                let plus = shift(base, delta).unwrap();
                let back = shift(plus, -delta).unwrap();
                assert_eq!(back, base, "base={base} delta={delta}");
            }
        }
    }
}
