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
import contextlib
import time
import wave
import voice
import polish
import cost_stats
import sessions
import ui_state as status_ui

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
    return sessions.transcribe(Path(directory), pcm, number, api_key)

def main():
    insert = '--insert' in sys.argv[1:]
    if insert:
        voice.STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
        try:
            with sessions.lock(voice.STATE):
                (voice.STATE / 'recording.pid').write_text(str(os.getpid()))
                try:
                    return run(insert)
                finally:
                    (voice.STATE / 'recording.pid').unlink(missing_ok=True)
        except RuntimeError:
            voice.notify('Предыдущая диктовка ещё обрабатывается.')
            return 1
    return run(insert)

def run(insert):
    stopped_at = None
    cancelled = False
    process = None
    def cancel(signum, frame):
        nonlocal cancelled
        cancelled = True
        stop(signum, frame)
    def check_cancel():
        if cancelled:
            raise InterruptedError("Диктовка отменена")
    def stop(signum, frame):
        nonlocal stopped_at
        if stopped_at is None:
            stopped_at = time.monotonic()
            if process is not None and process.poll() is None:
                process.send_signal(signal.SIGINT)
    signal.signal(signal.SIGUSR1, cancel)
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    os.umask(0o077)
    sessions.cleanup()
    api_key = voice.key()
    if not api_key:
        if insert:
            voice.notify('Нет ключа OpenRouter.')
        raise SystemExit('Ключ OpenRouter недоступен')
    voice.STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(voice.STATE, 0o700)
    window = voice.focused_window()
    session = sessions.create()
    if insert:
        status_ui.publish("recording", session.name)
        status_ui.start()
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
        with contextlib.ExitStack() as stack:
            stack.enter_context(sessions.lock(session))
            journal = stack.enter_context((session / 'audio.pcm').open('ab', buffering=0))
            pool = stack.enter_context(concurrent.futures.ThreadPoolExecutor(max_workers=3))
            tmp = str(session)
            while True:
                data = os.read(process.stdout.fileno(), 8192)
                if not data:
                    break
                journal.write(data)
                buffer.extend(data)
                while len(buffer) >= CHUNK_BYTES:
                    os.fsync(journal.fileno())
                    pcm = bytes(buffer[:CHUNK_BYTES])
                    del buffer[:CHUNK_BYTES]
                    futures.append(pool.submit(send_chunk, pcm, api_key, tmp, len(futures)))
            process.wait()
            os.fsync(journal.fileno())
            sessions.status(session, 'processing')
            if insert:
                status_ui.publish('cancelling' if cancelled else 'processing', session.name)
            check_cancel()
            if stopped_at is None:
                raise RuntimeError('Запись прервалась до остановки.')
            if len(buffer) >= 2:
                futures.append(pool.submit(send_chunk, bytes(buffer[:len(buffer) // 2 * 2]), api_key, tmp, len(futures)))
            if not futures:
                raise RuntimeError('Запись слишком короткая.')
            # Never insert an incomplete transcript when a request fails.
            results = [future.result() for future in futures]
            check_cancel()
            parts = [result[0] for result in results]
            text = ' '.join(part.strip() for part in parts if part.strip())
            if not text:
                raise RuntimeError('Сервис вернул пустой текст.')
            sessions.save_text(session, text)
            elapsed = time.monotonic() - stopped_at
            edit_seconds = None
            edit_status = 'disabled'
            edit_usage = None
            if insert and os.environ.get('VELO_POLISH', '1') != '0':
                try:
                    status_ui.publish('editing', session.name)
                    text, edit_seconds, edit_usage = sessions.retry(lambda: polish.polish(text, api_key), session / 'attempts-edit.jsonl')
                    sessions.save_text(session, text, 'edited.json')
                    sessions.write(session / 'edit-usage.json', edit_usage)
                    edit_status = 'ok'
                except (OSError, ValueError, KeyError, IndexError) as exc:
                    print(f'Редактура недоступна: {exc}', file=sys.stderr)
                    edit_status = 'failed'
            check_cancel()
            sessions.finish(session)
            if insert:
                try:
                    sessions.account(session, edit_status)
                except OSError as exc:
                    print(f'Не удалось сохранить статистику: {exc}', file=sys.stderr)
            check_cancel()
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
            if insert:
                status_ui.publish('ready', session.name)
            with (voice.STATE / 'chunked-timings.jsonl').open('a', encoding='utf-8') as f:
                f.write(json.dumps({'chunks': len(futures), 'stop_to_text_seconds': round(elapsed, 3),
                    'edit_seconds': round(edit_seconds, 3) if edit_seconds is not None else None,
                    'edit_status': edit_status, 'stop_to_delivery_seconds': round(time.monotonic() - stopped_at, 3)}) + '\n')
    except Exception as exc:
        sessions.status(session, 'cancelled' if cancelled else 'failed')
        if insert:
            status_ui.publish('cancelled' if cancelled else 'failed', session.name)
        try:
            sessions.account(session, 'failed')
        except OSError:
            pass
        print(f'Ошибка: {exc}', file=sys.stderr)
        if insert:
            voice.notify(f'Запись сохранена для восстановления: {session.name}')
        return 1
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait()
    return 0

if __name__ == '__main__':
    sys.exit(main())
