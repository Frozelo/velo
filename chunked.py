#!/usr/bin/env python3
"""Experimental chunked transcription; run, speak, then press Ctrl+C."""
import concurrent.futures
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import wave
import voice

RATE = 16000
CHUNK_BYTES = RATE * 2 * 5

def wav_bytes(pcm):
    output = io.BytesIO()
    with wave.open(output, 'wb') as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(RATE)
        wav.writeframes(pcm)
    return output.getvalue()

def send_chunk(pcm, api_key, directory, number):
    path = Path(directory) / f'{number}.wav'
    path.write_bytes(wav_bytes(pcm))
    return voice.transcribe(path, api_key, language='ru')

def main():
    insert = '--insert' in sys.argv[1:]
    if insert:
        try:
            fd = os.open(voice.STATE / 'recording.pid', os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileNotFoundError:
            voice.STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
            fd = os.open(voice.STATE / 'recording.pid', os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError:
            voice.notify('Предыдущая диктовка ещё обрабатывается.')
            return 1
        with os.fdopen(fd, 'w') as f:
            f.write(str(os.getpid()))
        try:
            return run(insert)
        finally:
            (voice.STATE / 'recording.pid').unlink(missing_ok=True)
    return run(insert)

def run(insert):
    stopped_at = None
    process = None
    def stop(signum, frame):
        nonlocal stopped_at
        if stopped_at is None:
            stopped_at = time.monotonic()
            if process is not None and process.poll() is None:
                process.send_signal(signal.SIGINT)
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    api_key = voice.key()
    if not api_key:
        if insert:
            voice.notify('Нет ключа OpenRouter.')
        raise SystemExit('Ключ OpenRouter недоступен')
    voice.STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(voice.STATE, 0o700)
    window = voice.focused_window()
    process = subprocess.Popen(['pw-record', '--raw', '--rate', str(RATE), '--channels', '1', '--format', 's16', '-'], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    if stopped_at is not None:
        process.send_signal(signal.SIGINT)
    if insert:
        voice.notify('Запись началась. Отпусти клавишу, чтобы закончить.')
    else:
        print('Говори. Нажми Ctrl+C, чтобы закончить.', flush=True)
    buffer = bytearray()
    futures = []
    try:
        with tempfile.TemporaryDirectory(prefix='chunked-', dir=voice.STATE) as tmp, concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            while True:
                data = os.read(process.stdout.fileno(), 8192)
                if not data:
                    break
                buffer.extend(data)
                while len(buffer) >= CHUNK_BYTES:
                    pcm = bytes(buffer[:CHUNK_BYTES])
                    del buffer[:CHUNK_BYTES]
                    futures.append(pool.submit(send_chunk, pcm, api_key, tmp, len(futures)))
            process.wait()
            if stopped_at is None:
                raise RuntimeError('Запись прервалась до остановки.')
            if len(buffer) >= 2:
                futures.append(pool.submit(send_chunk, bytes(buffer[:len(buffer) // 2 * 2]), api_key, tmp, len(futures)))
            if not futures:
                raise RuntimeError('Запись слишком короткая.')
            # Never insert an incomplete transcript when a request fails.
            parts = [future.result() for future in futures]
            text = ' '.join(part.strip() for part in parts if part.strip())
            if not text:
                raise RuntimeError('Сервис вернул пустой текст.')
            elapsed = time.monotonic() - stopped_at
            if insert:
                subprocess.run(['wl-copy', '--', text], check=True)
                current = voice.focused_window()
                if window and current and window.get('id') == current.get('id'):
                    try:
                        voice.paste(current)
                        voice.notify('Команда вставки отправлена.')
                    except (OSError, ImportError):
                        voice.notify('Не удалось вставить текст; он в буфере обмена.')
                else:
                    voice.notify('Фокус изменился: текст сохранён в буфер обмена.')
            else:
                print(f'Готово за {elapsed:.2f} с после остановки; частей: {len(futures)}.\n{text}', flush=True)
                print('Текст только показан в терминале, в активное окно не вставлен.')
            with (voice.STATE / 'chunked-timings.jsonl').open('a', encoding='utf-8') as f:
                f.write(json.dumps({'chunks': len(futures), 'stop_to_text_seconds': round(elapsed, 3)}) + '\n')
    except Exception as exc:
        print(f'Ошибка: {exc}', file=sys.stderr)
        if insert:
            voice.notify('Ошибка записи или распознавания. Текст не вставлен.')
        return 1
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait()
    return 0

if __name__ == '__main__':
    sys.exit(main())
