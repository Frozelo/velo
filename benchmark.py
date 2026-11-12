#!/usr/bin/env python3
"""Compare transcription models using one short microphone recording."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid
import wave
import voice

MODELS = ['openai/whisper-large-v3-turbo', 'openai/whisper-large-v3', 'elevenlabs/scribe-v2']

def request(audio, model, api_key):
    boundary = '----velo-' + uuid.uuid4().hex
    data = (f'--{boundary}\r\nContent-Disposition: form-data; name="model"\r\n\r\n{model}\r\n'
            f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="sample.wav"\r\n'
            f'Content-Type: audio/wav\r\n\r\n').encode() + audio + f'\r\n--{boundary}--\r\n'.encode()
    req = urllib.request.Request('https://openrouter.ai/api/v1/audio/transcriptions', data=data,
        headers={'Authorization': 'Bearer ' + api_key, 'Content-Type': 'multipart/form-data; boundary=' + boundary})
    start = time.monotonic()
    with urllib.request.urlopen(req, timeout=90) as response:
        result = json.load(response)
    return time.monotonic() - start, result

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seconds', type=int, default=12, choices=range(3, 61), metavar='3..60')
    parser.add_argument('--rounds', type=int, default=2, choices=range(1, 6), metavar='1..5')
    args = parser.parse_args()
    api_key = voice.key()
    if not api_key:
        raise SystemExit('Ключ OpenRouter недоступен.')
    print(f'Одна запись {args.seconds} с будет отправлена {len(MODELS) * args.rounds} раз. Запросы платные.', flush=True)
    input('Нажми Enter и приготовься говорить через 3 секунды... ')
    for n in (3, 2, 1):
        print(n, flush=True)
        time.sleep(1)
    with tempfile.TemporaryDirectory(prefix='velo-benchmark-', dir=os.environ.get('XDG_RUNTIME_DIR')) as tmp:
        path = Path(tmp) / 'sample.wav'
        print('Говори!', flush=True)
        recording = subprocess.run(['pw-record', '--rate', '16000', '--channels', '1', '--format', 's16', '--sample-count', str(args.seconds * 16000), str(path)])
        # Some pw-record versions return 1 after reaching --sample-count even
        # though they write a complete WAV. Validate the file itself instead.
        try:
            with wave.open(str(path), 'rb') as wav:
                duration = wav.getnframes() / wav.getframerate()
            if duration < args.seconds * 0.9:
                raise ValueError(f'запись слишком короткая: {duration:.1f} с')
        except (OSError, EOFError, wave.Error, ValueError, ZeroDivisionError) as exc:
            raise SystemExit(f'Ошибка записи (код {recording.returncode}): {exc}') from exc
        audio = path.read_bytes()
        print('Запись закончена. Сравниваю модели…', flush=True)
        results = []
        for round_no in range(args.rounds):
            # Rotate the order to reduce systematic first-request bias.
            order = MODELS[round_no % len(MODELS):] + MODELS[:round_no % len(MODELS)]
            for model in order:
                try:
                    elapsed, result = request(audio, model, api_key)
                    text = result.get('text', '')
                    usage = result.get('usage', {})
                    results.append({'model': model, 'round': round_no + 1, 'seconds': round(elapsed, 3), 'text': text, 'usage': usage})
                    print(f'\n{model} · попытка {round_no + 1} · {elapsed:.2f} с\n{text}', flush=True)
                except (OSError, ValueError, urllib.error.URLError) as exc:
                    print(f'\n{model} · ошибка: {exc}', flush=True)
        output = voice.STATE / 'benchmark.json'
        voice.STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
        fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, 'w', encoding='utf-8') as f:
            json.dump(results, f, ensure_ascii=False, indent=2)
        print(f'\nРезультаты сохранены локально: {output}. Аудиофайл удалён.')

if __name__ == '__main__':
    main()
