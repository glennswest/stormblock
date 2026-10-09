# Retries: every call that leaves the process (#359)

Owner (2026-10-08): "a single timeout must never kill us, and the bug is the
missing retry." This is the review the issue asked for: every call stormblock
makes to something outside its process, what it did before, and what it does
now. The helper is `src/retry.rs`.

## The helper

`retry::with_backoff(what, policy, classify, op)`:

- **Bounded:** at most `attempts` tries, and none starts past the
  whole-operation `deadline`.
- **Backoff with jitter:** `base`, doubling up to `max_delay`. Each wait is
  drawn from the upper half of its step, so callers that failed together
  don't retry together.
- **Only what is worth retrying:** the classifier answers `Transient` (a
  timeout, a refused, reset or dropped connection, a 5xx, 408 or 429) or
  `Permanent` (a 4xx, a validation error). `Permanent` fails at once.
- **Said in the log:** "succeeded on attempt 3 after 4.1s", or "gave up after
  5 attempt(s) / 60.3s: <last error>". A flaky dependency shows up in the log.
- **Classified for the caller:** `Failure::is_infrastructure()` tells "gave up
  on the network" from "got a real answer".

| policy | attempts | waits | deadline | for |
|---|---|---|---|---|
| `QUICK` | 3 | 100 ms → 1 s | 10 s | a call an API request waits on |
| `NETWORK` | 5 | 200 ms → 5 s | 60 s | another service over HTTP |
| `BLOCK_IO` | 3 | 100 ms → 1 s | 120 s | one NVMe/TCP or iSCSI command (each attempt has its own I/O timeout, `STORMBLOCK_NVME_TCP_IO_TIMEOUT_SECS`, 30 s) |
| `TRANSFER` | 8 | 1 s → 30 s | 30 min | an image download, resuming; an attempt that made progress starts the count and the deadline over (#125: `with_backoff_progress`), so a long download over a flaky link fails only when it stops getting anywhere |

Only idempotent operations go through the helper, or ones made safe to
repeat. A call that must not be retried says why where it is made.

**Transport is not media.** `DriveError::is_transport()` (a timeout, a
dropped connection) is never a reason to stop trusting the media
(`is_media_failure()` excludes it). An error that is still transport after the
device's own retries never marks the only copy of a volume failed. Before
this, one network blip left a single-copy volume unreadable until a restart.
On a redundant volume it degrades the leg, as a media error does, and a resync
takes over.

## The call sites

| where | what | before | now |
|---|---|---|---|
| `src/drive/nvmeof_dev.rs:783` (read, write, flush, discard) | NVMe/TCP block I/O to another engine or the forge | one attempt (bounded by #358's I/O timeout), the next op reconnects | `BLOCK_IO`: a transport error reconnects and tries again; a real answer fails at once |
| `src/drive/iscsi_dev.rs:728` (read, write, flush, discard) and `establish` | iSCSI block I/O | no deadline anywhere; a failed connection stayed installed | connect, login and every command bounded by the I/O timeout; a failure drops the session; the next attempt logs in again; `BLOCK_IO` |
| `src/drive/httpdev.rs:30` | Range GETs of a release image (staging, #122) | no timeout; its own loop of 5 tries, 500 ms apart | each GET bounded at 30 s; `NETWORK` for the open and every chunk; a 4xx not retried |
| `src/http.rs:269` `send()` | every GET/HEAD the engine makes | one attempt | `NETWORK`: retried on a transport error or a 5xx, 408 or 429, with the last answer returned. POST, PUT and DELETE are sent once unless the caller uses `send_retried` |
| `src/http.rs:368` `content_length` | HEAD before a streamed import | **no timeout**, one attempt | bounded by the client's timeout; `NETWORK` |
| `src/http.rs:465` `get_to_channel` | a raw image streamed into a volume | **no timeout** on the headers or the body; one attempt | headers and each frame bounded (an idle bound, not a total one); `TRANSFER`, resuming with a `Range` from the byte reached; a server that won't resume fails, and says so |
| `src/http.rs:505` `get_to_file` | an image downloaded to `imports/` | no timeout; one attempt; the partial file was left behind | bounded the same way; `TRANSFER`, resuming from the file's length (a server that answers 200 starts it over); the partial file is removed when it gives up |
| `src/image/import.rs:363` | the import's download | one attempt; a 6 h client timeout; `?` leaked the partial file | `get_to_file` (above) with a 2 min idle bound |
| `src/mgmt/kubeauth.rs:118` | TokenReview and SubjectAccessReview (#274) | one attempt | `QUICK`: these change nothing, and an API call waits on them |
| `src/stormfs.rs:135`, `:169` | StormFS register and deregister | one attempt | `NETWORK` and `QUICK`: an announcement made twice is the same announcement |
| `src/cluster/mod.rs:151` | cluster join | one attempt | `NETWORK`: a join names the node by id |
| `src/cluster/migration.rs:136` | migration chunk copy | one attempt | `NETWORK`: the same bytes to the same offset |
| `src/cluster/migration.rs:88` | migration's volume create | one attempt | **not retried**, commented: no id is sent, so a create whose answer was lost would make a second volume. A failed migration starts again whole |
| `src/cluster/heartbeat.rs:124` | heartbeat | one attempt per round | **not retried**, commented: the next round is the retry, and a peer is marked unreachable only after several misses |
| `src/cluster/replication.rs:177` | async replication | its own retry queue with attempts logged | kept as it is (cluster is opt-in, #209) |
| `src/cluster/replication.rs:129` | sync replication fan-out | one attempt per write | kept: a failed replica is the write's answer, and async replication retries it |
| `src/image/build.rs:982` | removing a temporary export after a build | one attempt, result ignored | `QUICK`: a DELETE is safe to repeat |
| `src/cli.rs:6899` | `boot-claim` | its own loop until `--timeout`, 404s named | kept: a claim is not idempotent (each makes a fresh clone), but the engine releases the clone a repeated claim replaces (#127), and the loop reads the claim's own answers |
| `src/cli.rs:7012` | the install report (#148) | its own loop for an hour, 4xx not retried | kept: idempotent, already bounded, and it stops on a 4xx |
| `src/mgmt/discovery.rs:311` | UDP discovery beacons | sent every interval | not retried: the next beacon is the retry |

Also found by the Codex review:

- **`src/volume/thin.rs:1043` `allocate_apart`** swallowed every allocation
  error, so a flush timeout read as "capacity exceeded". A failure that is not
  space is now logged and returned as itself.
- **`src/volume/thin.rs:732`** is the transport rule above.

## Tests

- `retry::tests`:
  - fails twice then succeeds on attempt 3;
  - a real answer is not retried;
  - infrastructure that stays down gives up bounded and says so;
  - the deadline caps the attempts;
  - statuses and errors are classified;
  - the delay grows and is capped.
- `http::tests::a_download_cut_off_resumes_and_a_503_is_tried_again`: a server
  that hangs up a third of the way in, then answers the `Range`; and a GET
  answered 503 twice.
- `integration_nvmeof::a_command_on_a_connection_that_went_silent_…`: a flush
  across a 2.5 s blip succeeds on a retry; one across a silence that doesn't
  end gives up bounded.
