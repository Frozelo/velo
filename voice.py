#!/usr/bin/env python3
"""Small Linux push-to-toggle dictation prototype using OpenRouter."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
import wave

STATE = Path(os.environ.get('XDG_RUNTIME_DIR', '/tmp')) / f'voice-dictation-{os.getuid()}'
MODEL = os.environ.get('VOICE_MODEL', 'openai/whisper-large-v3-turbo')

def notify(message):
    subprocess.run(['notify-send', 'Диктовка', message], check=False)

def focused_window():
    try:
        return json.loads(subprocess.check_output(['niri', 'msg', '-j', 'focused-window'], text=True, stderr=subprocess.DEVNULL))
    except (OSError, subprocess.CalledProcessError, ValueError):
        return None

def paste(window):
    from evdev import UInput, ecodes as e
    terminal = (window.get('app_id') or '').lower() in ('kitty', 'alacritty', 'org.gnome.terminal', 'com.mitchellh.ghostty')
    keys = [e.KEY_LEFTCTRL] + ([e.KEY_LEFTSHIFT] if terminal else []) + [e.KEY_V]
    with UInput({e.EV_KEY: keys}, name='Velo paste') as device:
        time.sleep(0.15)
        for code in keys:
            device.write(e.EV_KEY, code, 1)
            device.syn()
        for code in reversed(keys):
            device.write(e.EV_KEY, code, 0)
            device.syn()

def key():
    value = os.environ.get('OPENROUTER_API_KEY')
    if value:
        return value
    try:
        return subprocess.check_output(['secret-tool', 'lookup', 'application', 'voice-dictation', 'service', 'openrouter'], stderr=subprocess.DEVNULL, text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        return ''

def transcribe(audio, api_key, language=None):
    boundary = '----voice-' + uuid.uuid4().hex
    chunks = []
    def field(name, data, filename=None):
        disposition = f'Content-Disposition: form-data; name="{name}"'
        if filename:
            disposition += f'; filename="{filename}"\r\nContent-Type: audio/wav'
        chunks.append(f'--{boundary}\r\n{disposition}\r\n\r\n'.encode() + data + b'\r\n')
    field('model', MODEL.encode())
    if language:
        field('language', language.encode())
    field('file', audio.read_bytes(), 'recording.wav')
    chunks.append(f'--{boundary}--\r\n'.encode())
    request = urllib.request.Request('https://openrouter.ai/api/v1/audio/transcriptions', data=b''.join(chunks), headers={'Authorization': 'Bearer ' + api_key, 'Content-Type': 'multipart/form-data; boundary=' + boundary})
    with urllib.request.urlopen(request, timeout=90) as response:
        return json.load(response)['text'].strip()

def record():
    recorder = None
    stopping = False
    stopped_at = None
    def stop(signum, frame):
        nonlocal stopping, stopped_at
        if stopped_at is None:
            stopped_at = time.monotonic()
        stopping = True
        if recorder and recorder.poll() is None:
            recorder.send_signal(signal.SIGINT)
    signal.signal(signal.SIGTERM, stop)
    api_key = key()
    if not api_key:
        notify('Нет ключа OpenRouter. См. README.')
        return 1
    STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(STATE, 0o700)
    pidfile = STATE / 'recording.pid'
    try:
        fd = os.open(pidfile, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    except FileExistsError:
        print('Запись уже активна или остался файл состояния:', pidfile, file=sys.stderr)
        return 1
    with os.fdopen(fd, 'w') as f:
        f.write(str(os.getpid()))
    window = focused_window()
    try:
        with tempfile.TemporaryDirectory(prefix='voice-', dir=STATE) as tmp:
            audio = Path(tmp) / 'audio.wav'
            recorder = subprocess.Popen(['pw-record', '--rate', '16000', '--channels', '1', '--format', 's16', str(audio)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            notify('Запись началась. Нажми сочетание ещё раз, чтобы закончить.')
            if stopping:
                recorder.send_signal(signal.SIGINT)
            recorder.wait()
            recorded_at = time.monotonic()
            if not stopping or not audio.exists() or audio.stat().st_size < 1000:
                raise RuntimeError('Запись не получилась или слишком короткая.')
            notify('Распознаю речь…')
            started = time.monotonic()
            text = transcribe(audio, api_key)
            api_done = time.monotonic()
            if not text:
                raise RuntimeError('Сервис вернул пустой текст.')
            subprocess.run(['wl-copy', '--', text], check=True)
            current = focused_window()
            outcome = 'clipboard'
            if window and current and window.get('id') == current.get('id'):
                try:
                    paste(current)
                    outcome = 'paste_requested'
                    done = time.monotonic()
                    notify(f'Команда вставки отправлена ({time.monotonic() - started:.1f} с).')
                except (OSError, ImportError):
                    done = time.monotonic()
                    notify('Не удалось вставить текст; он в буфере обмена.')
            else:
                done = time.monotonic()
                notify('Фокус изменился: текст сохранён в буфер обмена.')
            with wave.open(str(audio), 'rb') as wav:
                audio_seconds = wav.getnframes() / wav.getframerate()
            metrics = {'model': MODEL, 'audio_seconds': round(audio_seconds, 3),
                       'stop_to_recorded_seconds': round(recorded_at - stopped_at, 3),
                       'api_seconds': round(api_done - started, 3),
                       'post_api_seconds': round(done - api_done, 3),
                       'signal_to_result_seconds': round(done - stopped_at, 3),
                       'outcome': outcome}
            with (STATE / 'timings.jsonl').open('a', encoding='utf-8') as timings:
                timings.write(json.dumps(metrics) + '\n')
            print('Транскрибация завершена.')
    except (OSError, RuntimeError, ValueError, KeyError, urllib.error.URLError, subprocess.CalledProcessError) as exc:
        print(f'Ошибка: {exc}', file=sys.stderr)
        notify('Ошибка записи или распознавания. Смотри журнал.')
        return 1
    finally:
        pidfile.unlink(missing_ok=True)
    return 0

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['toggle', 'record'])
    args = parser.parse_args()
    if args.action == 'record':
        return record()
    pidfile = STATE / 'recording.pid'
    if pidfile.exists():
        try:
            pid = int(pidfile.read_text())
            cmdline = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
            if not any(Path(os.fsdecode(item)).resolve() == Path(__file__).resolve() for item in cmdline if item and not item.startswith(b'-')):
                raise ProcessLookupError('stale pid')
            os.kill(pid, signal.SIGTERM)
            return 0
        except (ValueError, OSError):
            pidfile.unlink(missing_ok=True)
    STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    log = open(STATE / 'voice.log', 'a', encoding='utf-8')
    subprocess.Popen([sys.executable, str(Path(__file__).resolve()), 'record'], stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    return 0

if __name__ == '__main__':
    sys.exit(main())
