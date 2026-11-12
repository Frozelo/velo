#!/usr/bin/env python3
"""List, resume, copy or delete local Velo dictations."""
import argparse
import json
import os
import shutil
import subprocess
import sessions
import voice
import polish
from chunked import CHUNK_BYTES


def resume(path, api_key):
    metadata = json.loads((path / 'session.json').read_text())
    if metadata['model'] != voice.MODEL:
        raise ValueError('Для восстановления установи VOICE_MODEL=' + metadata['model'])
    if (path / 'raw.json').exists():
        return json.loads((path / 'raw.json').read_text())['text']
    audio = path / 'audio.pcm'
    if not audio.exists():
        raise ValueError('Нет записи для восстановления')
    parts = []
    with audio.open('rb') as f:
        number = 0
        while pcm := f.read(CHUNK_BYTES):
            pcm = pcm[:len(pcm) // 2 * 2]
            if not pcm:
                break
            text, _ = sessions.transcribe(path, pcm, number, api_key)
            parts.append(text)
            number += 1
    if not parts:
        raise ValueError('Сохранённая запись пуста')
    text = ' '.join(parts)
    sessions.save_text(path, text)
    return text


def main():
    os.umask(0o077)
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('action', choices=['list', 'retry', 'copy', 'delete', 'cleanup'])
    p.add_argument('session', nargs='?', default='last')
    args = p.parse_args()
    if args.action == 'cleanup':
        print('Удалено просроченных сессий:', sessions.cleanup())
        return
    if args.action == 'list':
        for item in sorted(sessions.ROOT.glob('*/session.json')):
            data = json.loads(item.read_text())
            print(item.parent.name, data['created'], data['status'])
        return
    path = sessions.resolve(args.session)
    with sessions.lock(path):
        if args.action == 'delete':
            shutil.rmtree(path)
            print('Сессия удалена')
            return
        if args.action == 'retry':
            api_key = voice.key()
            if not api_key:
                raise ValueError('Ключ OpenRouter недоступен')
            text = resume(path, api_key)
            if not (path / 'edited.json').exists() and os.environ.get('VELO_POLISH', '1') != '0':
                edited, edit_seconds, usage = sessions.retry(lambda: polish.polish(text, api_key), path / 'attempts-edit.jsonl')
                sessions.save_text(path, edited, 'edited.json')
                sessions.write(path / 'edit-usage.json', usage)
            sessions.account(path, 'ok' if (path / 'edited.json').exists() else 'disabled')
            sessions.finish(path)
            print('Восстановлено. Для копирования: python3 recover.py copy', path.name)
            return
        target = path / 'edited.json'
        if not target.exists():
            target = path / 'raw.json'
        if not target.exists():
            raise ValueError('Готового текста нет; сначала выполни retry')
        subprocess.run(['wl-copy', '--', json.loads(target.read_text())['text']], check=True)
        print('Текст скопирован в буфер обмена')


if __name__ == '__main__':
    try:
        main()
    except (OSError, RuntimeError, ValueError, KeyError) as exc:
        raise SystemExit(f'Ошибка: {exc}') from exc
