# Velo — прототип голосового ввода

Запись через PipeWire, транскрибация через OpenRouter и копирование в буфер Wayland. Первый прототип: Niri/Linux, без автоматической вставки и обработки текста. Аудио отправляется OpenRouter; временный WAV удаляется после обработки. Состояние и ошибки записываются в журнал в `XDG_RUNTIME_DIR/voice-dictation-UID/voice.log`, который доступен текущему пользователю. Успешный результат заменяет содержимое буфера обмена.

## Подготовка

Нужны Python 3, `pw-record`, `wl-copy`, `notify-send` и `secret-tool`. Установи API-ключ в системное хранилище, вводя его только в терминале (не в чат):

```sh
secret-tool store --label='Voice Dictation OpenRouter' application voice-dictation service openrouter
```

Введи ключ в появившемся запросе. Требуется работающий Secret Service. Альтернатива для запуска из терминала — переменная `OPENROUTER_API_KEY`; не добавляй ключ в репозиторий или конфиг Niri.

## Проверка

```sh
python3 ~/Projects/voice-dictation/voice.py toggle
# произнеси фразу; затем снова:
python3 ~/Projects/voice-dictation/voice.py toggle
```

Дождись уведомления и вставь текст из буфера. Журнал: `cat "$XDG_RUNTIME_DIR/voice-dictation-$(id -u)/voice.log"`. Запросы могут расходовать средства API. Модель по умолчанию — `openai/whisper-large-v3-turbo`; для запуска из терминала её можно заменить переменной `VOICE_MODEL`.

После проверки можно привязать к свободному сочетанию Niri команду `spawn "python3" "/абсолютный/путь/к/voice.py" "toggle";` внутри блока `binds`. Приложение ещё не проверено на macOS, в реальном API или с микрофоном.
