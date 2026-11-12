#!/usr/bin/env python3
"""Show local per-dictation OpenRouter costs; stores no transcript or audio."""
import json
import os
import argparse
from pathlib import Path
from datetime import datetime, timezone

DIR = Path(os.environ.get('XDG_STATE_HOME', Path.home() / '.local/state')) / 'velo'
FILE = DIR / 'costs.jsonl'

def cost(usage):
    value = usage.get('cost') if isinstance(usage, dict) else None
    return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) and value >= 0 else None

def save(chunks, editor, edit_status):
    amounts = [cost(item) for item in chunks]
    edit = cost(editor) if edit_status == 'ok' else (0.0 if edit_status == 'disabled' else None)
    speech = sum(x for x in amounts if x is not None)
    complete = all(x is not None for x in amounts) and edit is not None
    row = {'time': datetime.now(timezone.utc).isoformat(), 'chunks': len(chunks),
           'speech_usd': speech, 'edit_usd': edit, 'known_usd': speech + (edit or 0),
           'complete': complete, 'edit_status': edit_status}
    DIR.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd = os.open(FILE, os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
    with os.fdopen(fd, 'a', encoding='utf-8') as f:
        f.write(json.dumps(row) + '\n')
    return row

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rub-rate', type=float, help='курс рублей за доллар для пересчёта')
    args = parser.parse_args()
    if args.rub_rate is not None and args.rub_rate <= 0:
        parser.error('курс должен быть положительным')
    if not FILE.exists():
        print('Пока нет статистики. Продиктуй сообщение, затем запусти снова.')
        return
    rows = []
    with FILE.open(encoding='utf-8') as f:
        for line in f:
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    if not rows:
        print('Пока нет статистики.')
        return
    last = rows[-1]
    full = [r for r in rows if r.get('complete')]
    print(f"Последняя диктовка: ${last['known_usd']:.8f}" + ('' if last['complete'] else ' (неполные данные)'))
    print(f"  Распознавание: ${last['speech_usd']:.8f}; редактура: " + (f"${last['edit_usd']:.8f}" if last['edit_usd'] is not None else 'неизвестно'))
    print(f'Всего записей: {len(rows)}, с полной стоимостью: {len(full)}')
    if full:
        print(f"Средняя стоимость полной записи: ${sum(r['known_usd'] for r in full)/len(full):.8f}")
        print(f"Всего по полным записям: ${sum(r['known_usd'] for r in full):.8f}")
    if args.rub_rate is not None:
        print(f"Последняя: {last['known_usd'] * args.rub_rate:.4f} ₽ по курсу {args.rub_rate:g} ₽/$" + ('' if last['complete'] else ' (неполные данные)'))
        if full:
            print(f"Средняя по полным: {sum(r['known_usd'] for r in full) / len(full) * args.rub_rate:.4f} ₽")
    print('Данные о старых диктовках и неуспешных запросах могут отсутствовать.')

if __name__ == '__main__':
    main()
