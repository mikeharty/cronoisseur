use anyhow::{Context, Result, anyhow, bail};
use atty::Stream;
use clap::Parser;
use once_cell::sync::Lazy;
use owo_colors::OwoColorize;
use regex::Regex;
use serde::Serialize;
use shlex::try_quote;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const PATTERN_GUIDE: &[(&str, &str)] = &[
    ("daily at HH:MM", "daily at 05:30"),
    ("weekdays at HH:MM", "weekdays at 07:15"),
    ("weekends at HH:MM", "weekends at 19:05"),
    ("<days> at HH:MM", "monday wednesday at 03:00"),
    ("weekly on <days> at HH:MM", "weekly on fri at 02:45"),
    (
        "monthly on <dates> at HH:MM",
        "monthly on 1st and 15th at 04:00",
    ),
    ("on <dates> at HH:MM", "on 10,20 at 22:30"),
    ("every N minutes", "every 15 minutes"),
    ("every N hours", "every 2 hours"),
    ("hourly at :MM", "hourly at :10"),
    ("raw cron", "30 3 * * 1"),
];

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Translate natural language schedules into cron entries."
)]
struct Cli {
    /// Natural language schedule or raw cron expression
    #[arg(value_name = "expression", required_unless_present = "list_patterns")]
    expression: Option<String>,

    /// Comment that will be placed above the cron entry
    #[arg(short, long, value_name = "text")]
    comment: Option<String>,

    /// Optional cron file to append the entry to
    #[arg(short, long, value_name = "file", requires = "write")]
    file: Option<PathBuf>,

    /// Write the entry to the user's cron file (auto-detected unless --file is provided)
    #[arg(long)]
    write: bool,

    /// Preview without writing
    #[arg(long)]
    dry_run: bool,

    /// Emit JSON describing the entry
    #[arg(long)]
    json: bool,

    /// Disable color
    #[arg(long)]
    no_color: bool,

    /// Show phrasing patterns (and quit)
    #[arg(long)]
    list_patterns: bool,

    /// Environment key=val pairs to set before the entry
    #[arg(
        long = "env",
        value_name = "key=value",
        num_args = 0..,
        value_parser = parse_env_var
    )]
    env: Vec<EnvVar>,

    /// Command to schedule
    #[arg(
        value_name = "command",
        required_unless_present = "list_patterns",
        num_args = 1..,
        trailing_var_arg = true
    )]
    command: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct EnvVar {
    key: String,
    value: String,
}

#[derive(Debug, Clone, Serialize)]
struct CronSpec {
    minute: String,
    hour: String,
    day_of_month: String,
    month: String,
    day_of_week: String,
    explanation: String,
}

impl CronSpec {
    fn new(
        minute: impl Into<String>,
        hour: impl Into<String>,
        day_of_month: impl Into<String>,
        month: impl Into<String>,
        day_of_week: impl Into<String>,
        explanation: impl Into<String>,
    ) -> Self {
        Self {
            minute: minute.into(),
            hour: hour.into(),
            day_of_month: day_of_month.into(),
            month: month.into(),
            day_of_week: day_of_week.into(),
            explanation: explanation.into(),
        }
    }

    fn as_string(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.minute, self.hour, self.day_of_month, self.month, self.day_of_week
        )
    }
}

#[derive(Debug, Clone, Serialize)]
struct CronEntry {
    schedule: CronSpec,
    command: String,
    comment: Option<String>,
    env: Vec<EnvVar>,
}

#[derive(Debug, Serialize)]
struct JsonReport {
    cron: String,
    entry: CronEntry,
    file: Option<PathBuf>,
    wrote_file: bool,
    dry_run: bool,
}

struct Painter {
    enabled: bool,
}

impl Painter {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn accent(&self, text: impl std::fmt::Display) -> String {
        let raw = text.to_string();
        if self.enabled {
            raw.bright_cyan().to_string()
        } else {
            raw
        }
    }

    fn success(&self, text: impl std::fmt::Display) -> String {
        let raw = text.to_string();
        if self.enabled {
            raw.bright_green().to_string()
        } else {
            raw
        }
    }

    fn warn(&self, text: impl std::fmt::Display) -> String {
        let raw = text.to_string();
        if self.enabled {
            raw.bright_yellow().to_string()
        } else {
            raw
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  Caused by: {cause}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    let use_color = !cli.no_color && atty::is(Stream::Stdout);
    let painter = Painter::new(use_color);

    if cli.list_patterns {
        print_pattern_guide(&painter);
        return Ok(());
    }

    let expression = cli
        .expression
        .as_deref()
        .expect("expression is required unless --list-patterns is used");
    let schedule = parse_expression(expression)
        .with_context(|| format!("Could not parse expression `{expression}`"))?;

    let command = cli
        .command
        .iter()
        .map(|part| {
            try_quote(part)
                .map(|quoted| quoted.to_string())
                .map_err(|err| anyhow!("Invalid command segment `{part}`: {err}"))
        })
        .collect::<Result<Vec<_>>>()?
        .join(" ");

    let entry = CronEntry {
        schedule,
        command,
        comment: cli.comment.clone(),
        env: cli.env.clone(),
    };

    let cron_line = entry.schedule.as_string();
    let preview_block = render_entry(&entry);

    let mut wrote_file = false;
    let mut target_file = None;
    if cli.write {
        let path = cli
            .file
            .clone()
            .unwrap_or_else(detect_cron_file);
        target_file = Some(path.clone());
        if !cli.dry_run {
            append_entry(&path, &preview_block)?;
            wrote_file = true;
        }
    }

    if cli.json {
        let report = JsonReport {
            cron: cron_line,
            entry,
            file: target_file.clone(),
            wrote_file,
            dry_run: cli.dry_run,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print_summary(
        &painter,
        &entry,
        &preview_block,
        &cron_line,
        &cli,
        wrote_file,
        target_file.as_ref(),
    );

    Ok(())
}

fn print_pattern_guide(painter: &Painter) {
    println!("{}", painter.accent("Supported phrasing samples:"));
    for (syntax, example) in PATTERN_GUIDE {
        println!(
            "  - {:<28} {}",
            syntax,
            painter.success(format!("e.g. {}", example))
        );
    }
}

fn print_summary(
    painter: &Painter,
    entry: &CronEntry,
    preview: &str,
    cron_line: &str,
    cli: &Cli,
    wrote_file: bool,
    target_file: Option<&PathBuf>,
) {
    println!("{}", painter.accent("Parsed Input"));
    println!(
        "Schedule: {}  ({})",
        painter.success(cron_line),
        entry.schedule.explanation
    );
    println!("Command: {}", entry.command);
    if let Some(comment) = &entry.comment {
        println!("Comment: {}", comment);
    }
    if !entry.env.is_empty() {
        let env_preview = entry
            .env
            .iter()
            .map(|pair| format!("{}={}", pair.key, pair.value))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  Env      : {}", env_preview);
    }
    if let Some(path) = target_file {
        let status = if cli.dry_run {
            painter.warn("dry run - not written")
        } else if wrote_file {
            painter.success("written")
        } else {
            painter.warn("skipped")
        };
        println!("  File     : {} ({})", path.display(), status);
    }
    println!("");
    println!("{}", painter.accent("Preview Output"));
    println!("{preview}");
}

fn detect_cron_file() -> PathBuf {
    if let Ok(path) = env::var("CRONTAB") {
        return PathBuf::from(path);
    }

    let username = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string());

    let candidates = [
        format!("/var/spool/cron/crontabs/{username}"),
        format!("/var/spool/cron/{username}"),
        format!("/etc/cron.d/{username}"),
    ];

    for candidate in candidates {
        let path = PathBuf::from(&candidate);
        if path.exists() || path.parent().is_some_and(Path::exists) {
            return path;
        }
    }

    default_cron_file()
}

fn default_cron_file() -> PathBuf {
    let home = env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    home.join(".crontab")
}

fn render_entry(entry: &CronEntry) -> String {
    let mut lines = Vec::new();
    if let Some(comment) = &entry.comment {
        lines.push(format!("# {comment}"));
    }
    for env in &entry.env {
        lines.push(format!("{}={}", env.key, env.value));
    }
    lines.push(format!("{} {}", entry.schedule.as_string(), entry.command));
    lines.join("\n")
}

fn append_entry(path: &Path, block: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed creating {}", parent.display()))?;
        }
    }

    let mut payload = String::new();
    if path.exists() {
        let metadata = fs::metadata(path)
            .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
        if metadata.len() > 0 && !file_ends_with_newline(path)? {
            payload.push('\n');
        }
    }

    payload.push_str(block);
    payload.push('\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed opening {}", path.display()))?;
    file.write_all(payload.as_bytes())
        .with_context(|| format!("Failed writing to {}", path.display()))?;
    Ok(())
}

fn file_ends_with_newline(path: &Path) -> Result<bool> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    if metadata.len() == 0 {
        return Ok(true);
    }

    let mut file = File::open(path)
        .with_context(|| format!("Failed to open {} for newline inspection", path.display()))?;
    file.seek(SeekFrom::End(-1))
        .with_context(|| format!("Failed seeking within {}", path.display()))?;
    let mut buf = [0u8; 1];
    file.read_exact(&mut buf)
        .with_context(|| format!("Failed reading tail byte of {}", path.display()))?;
    Ok(buf[0] == b'\n')
}

fn parse_expression(expression: &str) -> Result<CronSpec> {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        bail!("The expression is empty");
    }

    if let Some(spec) = try_parse_raw(trimmed) {
        return Ok(spec);
    }

    let normalized = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = normalized
        .to_lowercase()
        .replace('–', "-")
        .replace('—', "-");

    if let Some(spec) = try_parse_every_minutes(&normalized) {
        return Ok(spec);
    }
    if let Some(spec) = try_parse_hourly(&normalized) {
        return Ok(spec);
    }
    if let Some(spec) = try_parse_every_hours(&normalized) {
        return Ok(spec);
    }
    if let Some(spec) = try_parse_daily(&normalized) {
        return Ok(spec);
    }
    if let Some(spec) = try_parse_weekdayish(&normalized) {
        return Ok(spec);
    }
    if let Some(spec) = try_parse_specific_days(&normalized) {
        return Ok(spec);
    }
    if let Some(spec) = try_parse_monthly(&normalized) {
        return Ok(spec);
    }
    if let Some(spec) = try_parse_on_days(&normalized) {
        return Ok(spec);
    }

    bail!("Unsupported phrasing. Use flag --list-patterns to list all supported shapes.")
}

fn try_parse_raw(input: &str) -> Option<CronSpec> {
    let parts: Vec<_> = input.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }

    if parts.iter().all(|segment| {
        segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "*?/,-".contains(c))
    }) {
        Some(CronSpec::new(
            parts[0],
            parts[1],
            parts[2],
            parts[3],
            parts[4],
            "Raw cron expression".to_string(),
        ))
    } else {
        None
    }
}

fn try_parse_every_minutes(input: &str) -> Option<CronSpec> {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^every\s+(?:(?P<n>\d+)\s+)?min(?:ute)?s?$").unwrap());
    RE.captures(input).map(|caps| {
        let amount = caps
            .name("n")
            .map(|m| m.as_str().parse::<u32>().unwrap_or(1))
            .unwrap_or(1)
            .max(1);
        let minute = if amount == 1 {
            "*".to_string()
        } else {
            format!("*/{amount}")
        };
        CronSpec::new(
            minute,
            "*",
            "*",
            "*",
            "*",
            format!("Every {amount} minute(s)"),
        )
    })
}

fn try_parse_hourly(input: &str) -> Option<CronSpec> {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^(?:hourly|every\s+hour)(?:\s+at\s+:(?P<m>\d{1,2}))?$").unwrap());
    RE.captures(input).map(|caps| {
        let minute = caps
            .name("m")
            .map(|m| m.as_str().parse::<u32>().unwrap_or(0).min(59))
            .unwrap_or(0);
        CronSpec::new(
            minute.to_string(),
            "*",
            "*",
            "*",
            "*",
            if minute == 0 {
                "Every hour on the hour".to_string()
            } else {
                format!("Every hour at :{:02}", minute)
            },
        )
    })
}

fn try_parse_every_hours(input: &str) -> Option<CronSpec> {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^every\s+(?P<n>\d+)\s+hours?(?:\s+at\s+:(?P<m>\d{1,2}))?$").unwrap()
    });
    RE.captures(input).map(|caps| {
        let amount = caps
            .name("n")
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(1);
        let minute = caps
            .name("m")
            .map(|m| m.as_str().parse::<u32>().unwrap_or(0).min(59))
            .unwrap_or(0);
        CronSpec::new(
            minute.to_string(),
            if amount == 1 {
                "*".to_string()
            } else {
                format!("*/{amount}")
            },
            "*",
            "*",
            "*",
            if minute == 0 {
                format!("Every {amount} hour(s)")
            } else {
                format!("Every {amount} hour(s) at :{:02}", minute)
            },
        )
    })
}

fn try_parse_daily(input: &str) -> Option<CronSpec> {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^(?:(?:every\s+)?day|daily)(?:\s+at\s+)?(?P<time>.+)$").unwrap());
    RE.captures(input).and_then(|caps| {
        let (hour, minute) = parse_time_fragment(caps.name("time")?.as_str())?;
        Some(CronSpec::new(
            minute.to_string(),
            hour.to_string(),
            "*",
            "*",
            "*",
            format!("Daily at {}", format_clock(hour, minute)),
        ))
    })
}

fn try_parse_weekdayish(input: &str) -> Option<CronSpec> {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^(?:(?:every\s+)?(?P<kind>weekdays?|weekends?))\s+(?:at\s+)?(?P<time>.+)$")
            .unwrap()
    });
    RE.captures(input).and_then(|caps| {
        let (hour, minute) = parse_time_fragment(caps.name("time")?.as_str())?;
        let kind = caps.name("kind")?.as_str();
        let (dow, label) = if kind.starts_with("weekend") {
            ("6,0".to_string(), "weekends".to_string())
        } else {
            ("1-5".to_string(), "weekdays".to_string())
        };
        Some(CronSpec::new(
            minute.to_string(),
            hour.to_string(),
            "*",
            "*",
            dow,
            format!("{} at {}", capitalize(&label), format_clock(hour, minute)),
        ))
    })
}

fn try_parse_specific_days(input: &str) -> Option<CronSpec> {
    let (prefix, time_part) = input.split_once(" at ")?;
    let dow_set = parse_day_list(prefix)?;
    let (hour, minute) = parse_time_fragment(time_part)?;
    let explanation = format!(
        "{} at {}",
        describe_days(&dow_set.days),
        format_clock(hour, minute)
    );
    Some(CronSpec::new(
        minute.to_string(),
        hour.to_string(),
        "*",
        "*",
        dow_set.cron_value,
        explanation,
    ))
}

fn try_parse_monthly(input: &str) -> Option<CronSpec> {
    if !input.starts_with("monthly") {
        return None;
    }
    let remainder = input.trim_start_matches("monthly").trim();
    if remainder.is_empty() {
        return None;
    }

    if let Some(rest) = remainder.strip_prefix("on ") {
        let (dom_part, time_part) = rest.split_once(" at ")?;
        let dom = parse_dom_list(dom_part)?;
        let (hour, minute) = parse_time_fragment(time_part)?;
        let explanation = format!(
            "Monthly on {} at {}",
            dom.human_value,
            format_clock(hour, minute)
        );
        return Some(CronSpec::new(
            minute.to_string(),
            hour.to_string(),
            dom.cron_value,
            "*",
            "*",
            explanation,
        ));
    }

    if let Some(time_part) = remainder.strip_prefix("at ") {
        let (hour, minute) = parse_time_fragment(time_part)?;
        return Some(CronSpec::new(
            minute.to_string(),
            hour.to_string(),
            "1",
            "*",
            "*",
            format!(
                "Monthly on day 1 at {} (default day)",
                format_clock(hour, minute)
            ),
        ));
    }

    None
}

fn try_parse_on_days(input: &str) -> Option<CronSpec> {
    if !input.starts_with("on ") {
        return None;
    }
    let remainder = input.trim_start_matches("on ").trim();
    let (dom_part, time_part) = remainder.split_once(" at ")?;
    let dom = parse_dom_list(dom_part)?;
    let (hour, minute) = parse_time_fragment(time_part)?;
    Some(CronSpec::new(
        minute.to_string(),
        hour.to_string(),
        dom.cron_value,
        "*",
        "*",
        format!("On {} at {}", dom.human_value, format_clock(hour, minute)),
    ))
}

struct DayList {
    cron_value: String,
    days: Vec<u8>,
}

fn parse_day_list(prefix: &str) -> Option<DayList> {
    let normalized = prefix
        .replace(',', " ")
        .replace('&', " ")
        .replace(" and ", " ");
    let stop_words = ["every", "each", "on", "week", "weeks", "weekly", "the"];
    let mut days = Vec::new();
    for token in normalized.split_whitespace() {
        let lower = token.trim().to_lowercase();
        if stop_words.contains(&lower.as_str()) {
            continue;
        }
        let cleaned = if lower.ends_with('s') {
            &lower[..lower.len() - 1]
        } else {
            lower.as_str()
        };
        if let Some(value) = day_number(cleaned) {
            if !days.contains(&value) {
                days.push(value);
            }
        } else {
            return None;
        }
    }
    if days.is_empty() {
        return None;
    }
    days.sort();
    let cron_value = days
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(",");
    Some(DayList { cron_value, days })
}

fn day_number(token: &str) -> Option<u8> {
    match token {
        "sun" | "sunday" => Some(0),
        "mon" | "monday" => Some(1),
        "tue" | "tues" | "tuesday" => Some(2),
        "wed" | "weds" | "wednesday" => Some(3),
        "thu" | "thur" | "thurs" | "thursday" => Some(4),
        "fri" | "friday" => Some(5),
        "sat" | "saturday" => Some(6),
        _ => None,
    }
}

struct DomList {
    cron_value: String,
    human_value: String,
}

fn parse_dom_list(raw: &str) -> Option<DomList> {
    let normalized = raw
        .replace(',', " ")
        .replace(" and ", " ")
        .replace("th", "")
        .replace("rd", "")
        .replace("nd", "")
        .replace("st", "");
    let mut values = Vec::new();
    for token in normalized.split_whitespace() {
        if token.chars().all(|c| !c.is_ascii_digit()) {
            continue;
        }
        let digits = token
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            continue;
        }
        if let Ok(value) = digits.parse::<u32>() {
            if (1..=31).contains(&value) && !values.contains(&value) {
                values.push(value);
            }
        }
    }
    if values.is_empty() {
        return None;
    }
    values.sort();
    let cron_value = values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let human_value = values
        .iter()
        .map(|v| format!("{v}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(DomList {
        cron_value,
        human_value,
    })
}

fn parse_time_fragment(raw: &str) -> Option<(u32, u32)> {
    let trimmed = raw.trim().to_lowercase();
    if trimmed == "midnight" {
        return Some((0, 0));
    }
    if trimmed == "noon" {
        return Some((12, 0));
    }

    let mut fragment = trimmed.replace(' ', "");
    let mut meridian = None;
    if let Some(rest) = fragment.strip_suffix("am") {
        fragment = rest.to_string();
        meridian = Some("am");
    } else if let Some(rest) = fragment.strip_suffix("pm") {
        fragment = rest.to_string();
        meridian = Some("pm");
    }

    let mut parts = fragment.split(':');
    let hour_part = parts.next()?;
    let minute_part = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let hour = hour_part.parse::<u32>().ok()?;
    if hour > 23 {
        return None;
    }
    let minute = match minute_part {
        Some(value) => value.parse::<u32>().ok()?,
        None => 0,
    };
    if minute > 59 {
        return None;
    }

    let mut hour = hour;
    if let Some(marker) = meridian {
        if hour > 12 {
            return None;
        }
        if marker == "am" {
            if hour == 12 {
                hour = 0;
            }
        } else if hour != 12 {
            hour += 12;
        }
    }

    Some((hour, minute))
}

fn format_clock(hour: u32, minute: u32) -> String {
    format!("{:02}:{:02}", hour, minute)
}

fn describe_days(days: &[u8]) -> String {
    let labels = days
        .iter()
        .map(|d| match d {
            0 => "Sundays",
            1 => "Mondays",
            2 => "Tuesdays",
            3 => "Wednesdays",
            4 => "Thursdays",
            5 => "Fridays",
            _ => "Saturdays",
        })
        .collect::<Vec<_>>();
    if labels.len() == 1 {
        labels[0].to_string()
    } else {
        labels.join(", ")
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn parse_env_var(raw: &str) -> Result<EnvVar, String> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| "Expected key=value".to_string())?;
    if key.trim().is_empty() {
        return Err("Environment key cannot be empty".into());
    }
    Ok(EnvVar {
        key: key.trim().to_string(),
        value: value.trim().to_string(),
    })
}
