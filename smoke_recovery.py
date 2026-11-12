#!/usr/bin/env python3
"""Live, opt-in recovery check using one ten-second microphone recording."""
import json
import os
import subprocess
import time
from unittest.mock import patch
import voice
import sessions
import recover
from chunked import CHUNK_BYTES


def main():
    os.umask(0o077)
    api_key = voice.key()
    if not api_key:
        raise SystemExit('Ключ OpenRouter недоступен')
    print('Тест: 10 секунд записи, два платных запроса распознавания. Буфер обмена не меняется.', flush=True)
    input('Нажми Enter. Через 3 секунды начнётся запись: ')
    for n in (3, 2, 1):
        print(n, flush=True)
        time.sleep(1)
    path = sessions.create()
    print('Говори!', flush=True)
    with sessions.lock(path):
        audio = path / 'audio.pcm'
        with audio.open('wb') as f:
            result = subprocess.run(['pw-record', '--raw', '--rate', '16000', '--channels', '1', '--format', 's16', '--sample-count', '160000', '-'], stdout=f)
            f.flush()
            os.fsync(f.fileno())
        pcm = audio.read_bytes()
        if len(pcm) < CHUNK_BYTES * 2:
            raise SystemExit(f'Запись неполная (код {result.returncode}); сессия сохранена: {path.name}')
        print('Первый фрагмент отправляется в OpenRouter…', flush=True)
        sessions.transcribe(path, pcm[:CHUNK_BYTES], 0, api_key)
        cached = path / 'chunk-000000.json'
        first_bytes = cached.read_bytes()
        # Fail before the next request is sent: no unknown provider charge.
        try:
            raise ConnectionError('Контролируемый сбой перед отправкой второго фрагмента')
        except ConnectionError:
            sessions.status(path, 'failed')
        print('Сбой имитирован. Запись и первый результат на диске. Восстанавливаю…', flush=True)
        original = voice.transcribe
        with patch.object(voice, 'transcribe', wraps=original) as requests:
            text = recover.resume(path, api_key)
            calls = requests.call_count
        if calls != 1 or cached.read_bytes() != first_bytes:
            raise RuntimeError('Проверка повторной отправки не пройдена')
        sessions.account(path, 'disabled')
        sessions.finish(path)
        report = {'session': path.name, 'passed': True, 'requests_during_recovery': calls,
                  'first_chunk_unchanged': True, 'audio_removed': not audio.exists()}
        sessions.write(path / 'smoke-report.json', report)
        print('\nВосстановленный текст:\n' + text)
        print('\nПроверка пройдена: первый результат использован повторно, отправлен только второй фрагмент.')
        print('Сессия:', path.name)
        print('Аудио удалено; текст можно скопировать командой recover.py copy с этим ID.')


if __name__ == '__main__':
    try:
        main()
    except (OSError, RuntimeError, ValueError) as exc:
        raise SystemExit(f'Ошибка: {exc}') from exc
