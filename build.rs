fn main() {
    let datetime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| {
            let total_secs = d.as_secs();
            let secs_per_day = 86400;
            let days = total_secs / secs_per_day;
            let time_secs = total_secs % secs_per_day;
            let hours = time_secs / 3600;
            let minutes = (time_secs % 3600) / 60;
            let seconds = time_secs % 60;

            let mut y = 1970i64;
            let mut remaining = days as i64;
            loop {
                let days_in_year = if is_leap(y) { 366 } else { 365 };
                if remaining < days_in_year {
                    break;
                }
                remaining -= days_in_year;
                y += 1;
            }
            let months = [31, if is_leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
            let mut m = 1;
            for &md in &months {
                if remaining < md {
                    break;
                }
                remaining -= md;
                m += 1;
            }
            let d = remaining + 1;
            format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", y, m, d, hours, minutes, seconds)
        })
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_DATETIME={}", datetime);
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
