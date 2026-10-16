# Docker Compose Updater

Docker Compose Updater is a command line program that updates a Compose stack with freshly pulled images, checks container health, and attempts to restore the previously running image versions if the update fails or containers become unhealthy. The program is meant to be used with image tags that allow newer versions to be pulled, e.g. "latest". The program doesn't modify the Compose file unless an update fails in which case the previous image digests are hard coded to the file in order to force a downgrade.

## Build

```sh
cargo build --release
```

## Usage

```sh
docker-compose-updater \
  --compose-file ./compose.yaml \
  --wait-seconds 15 \
  --smtp-url 'smtps://user:password@smtp.example.com:465' \
  --email-from updates@example.com \
  --email-to ops@example.com
```

`--compose-file` and `--wait-seconds` are required. Container health is polled every `--wait-seconds` for up to `--max-wait-seconds`.

Email settings are optional, but `--smtp-url`, `--email-from`, and `--email-to` must be supplied together. Configured email is used for notifying about update results, errors and warnings.

## Caveats

- Locally built images don't have fallback support.
- Newer versions of containers might modify files in, e.g., bind mounts which might lead to older containers failing as well after an unsuccessful update and a fallback.
