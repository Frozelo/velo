#!/usr/bin/env python3
"""Compact, non-focusing GTK dictation capsule."""
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import cairo
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib, Gdk
import sessions
import voice

CSS = b'''
window { background: transparent; }
.capsule { background: #17181b; border: none; border-radius: 18px; padding: 3px 5px; }
.status { color: #f0efed; font-size: 13px; font-weight: 500; }
button { background: transparent; background-image: none; border: none; box-shadow: none; color: #82848c; border-radius: 16px; min-width: 24px; min-height: 24px; padding: 0; }
button:hover { background: #34353a; color: #ffffff; }
button:disabled { color: #55565c; }
menu { background: #232428; color: #eeeeee; border-radius: 10px; padding: 5px; }
menuitem { padding: 7px 12px; }
menuitem:hover { background: #393a40; }
'''


class Activity(Gtk.DrawingArea):
    def __init__(self):
        super().__init__()
        self.set_size_request(34, 24)
        self.phase = 'idle'
        self.connect('draw', self.draw_activity)
        GLib.timeout_add(80, self.tick)

    def tick(self):
        if self.get_mapped() and self.phase in ('recording', 'processing', 'editing', 'cancelling'):
            self.queue_draw()
        return True

    def draw_activity(self, _, cr):
        width, height = self.get_allocated_width(), self.get_allocated_height()
        if self.phase == 'failed':
            cr.set_source_rgb(.95, .48, .43)
        else:
            cr.set_source_rgb(.65, .66, .69)
        cr.set_line_cap(cairo.LINE_CAP_ROUND)
        cr.set_line_width(2)
        t = time.monotonic()
        for i in range(5):
            if self.phase == 'recording':
                size = 3 + 9 * (0.5 + 0.5 * math.sin(t * 5 + i * 1.1))
            elif self.phase in ('processing', 'editing', 'cancelling'):
                size = 3 + 4 * (0.5 + 0.5 * math.sin(t * 3 - i * .65))
            else:
                size = 4
            x = width / 2 + (i - 2) * 5
            cr.move_to(x, (height - size) / 2)
            cr.line_to(x, (height + size) / 2)
            cr.stroke()


class Panel(Gtk.Window):
    def __init__(self):
        super().__init__(title='Velo')
        self.set_wmclass('velo-status', 'Velo')
        self.set_accept_focus(False)
        self.set_focus_on_map(False)
        self.set_decorated(False)
        self.set_opacity(0.65)
        self.add_events(Gdk.EventMask.ENTER_NOTIFY_MASK | Gdk.EventMask.LEAVE_NOTIFY_MASK)
        self.connect('enter-notify-event', lambda *_: self.set_opacity(0.95) or False)
        self.connect('leave-notify-event', lambda *_: self.set_opacity(0.65) or False)
        self.set_resizable(False)
        self.set_keep_above(True)
        self.set_type_hint(Gdk.WindowTypeHint.UTILITY)
        self.set_app_paintable(True)
        visual = self.get_screen().get_rgba_visual()
        if visual:
            self.set_visual(visual)
        provider = Gtk.CssProvider()
        provider.load_from_data(CSS)
        Gtk.StyleContext.add_provider_for_screen(self.get_screen(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=5)
        box.get_style_context().add_class('capsule')
        self.add(box)
        self.activity = Activity()
        box.pack_start(self.activity, False, False, 0)
        self.cancel = self.button('process-stop-symbolic', 'Отменить диктовку', self.on_cancel)
        box.pack_start(self.cancel, False, False, 0)
        menu = self.button('view-more-symbolic', 'Последний текст и настройки панели', self.on_menu)
        box.pack_start(menu, False, False, 0)
        self.last = None
        self.data = {}
        self.hide_at = None
        self.connect('delete-event', lambda *_: self.hide() or True)
        self.show_all()
        GLib.timeout_add(250, self.refresh)

    def button(self, icon, tooltip, callback):
        button = Gtk.Button()
        button.set_image(Gtk.Image.new_from_icon_name(icon, Gtk.IconSize.MENU))
        button.set_tooltip_text(tooltip)
        button.get_accessible().set_name(tooltip)
        button.set_can_focus(False)
        button.connect('clicked', callback)
        return button

    def refresh(self):
        show = voice.STATE / 'panel-show'
        if show.exists():
            show.unlink(missing_ok=True)
            self.hide_at = None
            self.show_all()
        try:
            data = json.loads((voice.STATE / 'ui.json').read_text())
        except (OSError, ValueError):
            self.cancel.set_sensitive(False)
            return True
        self.data = data
        phase = data['phase']
        names = {'recording': 'Слушаю', 'processing': 'Распознаю', 'editing': 'Оформляю текст',
                 'ready': 'Готово', 'failed': 'Запись сохранена', 'cancelled': 'Отменено', 'cancelling': 'Отмена…'}
        self.activity.set_tooltip_text(names.get(phase, 'Готов к диктовке'))
        self.activity.get_accessible().set_name(names.get(phase, 'Готов к диктовке'))
        self.activity.phase = phase
        self.cancel.set_sensitive(phase in ('recording', 'processing', 'editing'))
        if (data.get('session'), phase) != self.last:
            previous = self.last
            self.last = (data.get('session'), phase)
            self.activity.queue_draw()
            if phase == 'recording':
                self.hide_at = None
                self.show_all()
            elif previous is not None and phase in ('ready', 'cancelled'):
                self.hide_at = time.monotonic() + 2
        if self.hide_at is not None and time.monotonic() >= self.hide_at:
            self.hide()
            self.hide_at = None
        return True

    def message(self, text):
        self.set_tooltip_text(text)
        self.get_accessible().set_name('Velo · ' + text)

    def on_cancel(self, _):
        try:
            pid = int(self.data['pid'])
            command = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
            if str(Path(__file__).with_name('chunked.py')).encode() not in command:
                raise ValueError('Процесс диктовки уже завершён')
            os.kill(pid, signal.SIGUSR1)
            self.message('Отмена…')
            self.cancel.set_sensitive(False)
        except (OSError, ValueError, KeyError):
            self.message('Нет активной записи')

    def on_menu(self, button):
        menu = Gtk.Menu()
        for label, action in [('Скопировать последний текст', self.on_copy), ('Скрыть панель', lambda _: self.hide())]:
            item = Gtk.MenuItem(label=label)
            item.connect('activate', action)
            menu.append(item)
        menu.show_all()
        menu.popup_at_widget(button, Gdk.Gravity.NORTH_EAST, Gdk.Gravity.SOUTH_EAST, None)

    def on_copy(self, _):
        choices = sorted(sessions.ROOT.glob('*/raw.json'), key=lambda p: p.stat().st_mtime)
        if not choices:
            self.message('Пока нет текста')
            return
        path = choices[-1].parent
        result = subprocess.run([sys.executable, str(Path(__file__).with_name('recover.py')), 'copy', path.name], capture_output=True)
        self.message('Текст скопирован' if result.returncode == 0 else 'Ошибка копирования')


def main():
    GLib.set_prgname('velo-status')
    voice.STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    (voice.STATE / 'panel-show').touch()
    try:
        with sessions.lock(voice.STATE / 'panel'):
            Panel()
            Gtk.main()
    except RuntimeError:
        pass


if __name__ == '__main__':
    (voice.STATE / 'panel').mkdir(mode=0o700, parents=True, exist_ok=True)
    main()
