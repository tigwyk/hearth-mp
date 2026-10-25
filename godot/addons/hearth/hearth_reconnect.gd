class_name HearthReconnect
## Sans-IO reconnect policy, mirroring `ReconnectPolicy` in `hearth-client`.
##
## After a socket drops, `decide()` says whether to retry and when. This script
## and the Rust core are both checked against
## `crates/hearth-client/tests/fixtures/reconnect_cases.json`.

## Give up once this long has passed since the first drop.
const WINDOW_MS := 30_000
## Delay before the first retry.
const BASE_MS := 500
## Retry delay cap.
const MAX_MS := 8_000


## The retry time in milliseconds, or `-1` to give up. `attempt` is 0-based.
static func decide(code: int, first_drop_ms: int, attempt: int, now_ms: int) -> int:
	# 1000 is a deliberate close; 4000-4999 are application-fatal (for example
	# 4001 SCHEMA_MISMATCH), so retrying would spin forever.
	if code == 1000 or (code >= 4000 and code <= 4999):
		return -1
	if now_ms - first_drop_ms >= WINDOW_MS:
		return -1
	# Cap the shift before it can overflow; MAX_MS caps the result anyway.
	var delay: int = mini(BASE_MS << mini(attempt, 20), MAX_MS)
	return now_ms + delay
