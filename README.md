# cronoisseur

Cronoisseur is a small utility that translates natural-language into cron entries.

```bash
cronoisseur "weekdays at 07:15" --comment "Morning sync" --dry-run -- ./sync.sh
```

## Features
- Understands phrases like `daily at 05:30`, `weekdays at 07:15`, `every 15 minutes`, or raw cron.
- Supports optional comments, environment variables, and JSON output for scripting.
- Auto-detects your crontab's path and writes to it (optional).

## Quick start
```bash
# Run from source
cargo run -- "weekdays at 07:15" --comment "Morning sync" --file ./cron.d/sync --env PATH=/usr/local/bin -- my-sync-command --verbose

# Install the binary locally
cargo install --path .
cronoisseur "every 30 minutes" --dry-run -- env COMMAND=backup /usr/local/bin/backup.sh
```

## Usage
- The first argument is the schedule expression (natural language or raw cron).
- The remaining positional arguments form the command to run; everything after the command starts is treated as part of the command.
- Helpful flags: `--comment <text>`, `--write`, `--file <path>` (overrides the auto-detected cron path), `--dry-run`, `--json`, `--env <key=value>` (repeatable), `--no-color`, `--list-patterns`.

## Examples
```bash
# Preview a daily backup without writing a file
cronoisseur "daily at 02:00" --comment "Nightly backup" --dry-run -- tar -czf /backups/site.tar.gz /var/www

# Write to cron with environment variables
cronoisseur "weekly on fri at 03:30" --file /etc/cron.d/weekly-maint --env PATH=/usr/local/bin --env RUST_LOG=info -- systemctl restart my-service

# JSON output
cronoisseur --json "every 10 minutes" -- echo "tick"
```

## Supported phrasing
Run `cronoisseur --list-patterns` to see accepted shapes and examples.

```bash
  - monthly on <dates> at HH:MM  e.g. monthly on 1st and 15th at 04:00
  - on <dates> at HH:MM          e.g. on 10,20 at 22:30
  - every N minutes              e.g. every 15 minutes
  - every N hours                e.g. every 2 hours
  - hourly at :MM                e.g. hourly at :10
  - raw cron                     e.g. 30 3 * * 1
```
