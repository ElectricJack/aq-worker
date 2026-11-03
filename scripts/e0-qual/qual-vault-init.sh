#!/bin/sh
# Install the generated qualification vault into this Compose project's own
# qual_vault volume. Never touches the E0 dev_vault or another project's volume.
set -eu
umask 077
install -d -o 10001 -g 10001 -m 0700 /qual-vault
publish() {
    source=$1
    target=$2
    { test -f "$source" && test ! -L "$source"; } || { echo 'qualification source missing' >&2; exit 1; }
    test ! -L "$target" || { echo 'refusing a symlink in the qualification vault' >&2; exit 1; }
    temporary=$(mktemp "/qual-vault/.qual-init.XXXXXX")
    install -o 10001 -g 10001 -m 0600 "$source" "$temporary"
    mv -f "$temporary" "$target"
}
if [ -f /qual-src/config.toml ]; then
    publish /qual-src/config.toml /qual-vault/config.toml
    for secret in /qual-src/secrets/*; do
        test -e "$secret" || continue
        publish "$secret" "/qual-vault/$(basename "$secret")"
    done
fi
