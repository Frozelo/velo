#!/usr/bin/env bash
set -euo pipefail
native_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
unit=velo-native-test.service
case "${1:-start}" in
    stop)
        systemctl --user stop "$unit"
        echo 'Rust-тест остановлен; исходный сервис восстановлен, если работал до теста.'
        exit 0
        ;;
    status)
        systemctl --user status "$unit" --no-pager
        exit 0
        ;;
    start) ;;
    *) echo 'Использование: test-live.sh [start|stop|status]' >&2; exit 2 ;;
esac
binary="$native_dir/target/release/velo-native"
if [[ ! -x "$binary" ]]; then
    echo 'Сначала собери cargo build --release --manifest-path native/Cargo.toml' >&2
    exit 1
fi
if systemctl --user is-active --quiet "$unit"; then
    echo 'Rust-тест уже запущен.'
    exit 0
fi
restore=()
if systemctl --user is-active --quiet velo-hold.service; then
    restore=(--property='ExecStopPost=/usr/bin/systemctl --user start velo-hold.service')
    systemctl --user stop velo-hold.service
fi
if ! systemd-run --user --collect --unit="$unit" "${restore[@]}" \
    --property=TimeoutStopSec=5s --property=RuntimeMaxSec=15min \
    --setenv="VELO_PROVIDER=${VELO_PROVIDER:-plusvibe}" "$binary" --hold; then
    if (( ${#restore[@]} )); then systemctl --user start velo-hold.service; fi
    exit 1
fi
echo 'Rust запущен на 15 минут; удерживай правую Cmd для диктовки.'
echo 'Для завершения: bash native/test-live.sh stop'
