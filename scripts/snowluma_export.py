#!/usr/bin/env python3
"""Snapshot the SnowLuma message store and export it as schema v1 history records (#58).

    python -I scripts/snowluma_export.py snapshot --live LIVE_DB --copy COPY_DB
    python -I scripts/snowluma_export.py export --db COPY_DB --out OUT.jsonl [--conv group:<id> ...]

snapshot opens the live store read-only (mode=ro, query_only, no checkpoint on close where the
SQLite build allows it) and copies it with SQLite's online backup, which includes committed WAL
pages. It refuses a copy inside the live store's directory and a live store whose -wal or -shm
is missing (a read-only open would create one beside the production database). It fails, and
removes the copy, if the live directory's file names change at open, during or after the backup,
or on any error, and it seals a good copy with COPY.snapshot.json (SHA-256). export reads only
a sealed, unchanged copy and writes records ordered by time; lone surrogates become U+FFFD.
Output stays on the Windows host; strip it before anything reaches the vault.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
from collections import Counter
import sys

REQUIRED = ('message_hash', 'is_group', 'session_id', 'sequence', 'private_direction', 'timestamp', 'data')
MEDIA = {'image': 'image', 'video': 'video', 'record': 'voice', 'file': 'file', 'flash_file': 'file',
         'mface': 'sticker', 'json': 'card', 'xml': 'card', 'markdown': 'markdown'}
CQ_FULL = re.compile(r'\[CQ:([A-Za-z_]+)((?:,[^,\]]*)*)\]')
CQ_HEAD = re.compile(r'\[CQ:([A-Za-z_]+)')
CARD_TYPES = {'json', 'xml', 'markdown'}
SURROGATE = re.compile(r'[\ud800-\udfff]')


def unescape(text):
    return text.replace('&#44;', ',').replace('&#91;', '[').replace('&#93;', ']').replace('&amp;', '&')


def parse_cq(message):
    """A card code or an unparsable code ends the message: its payload never becomes text."""
    items, at = [], 0
    while True:
        start = message.find('[CQ:', at)
        if start < 0:
            break
        if start > at:
            items.append({'type': 'text', 'data': {'text': unescape(message[at:start])}})
        head, full = CQ_HEAD.match(message, start), CQ_FULL.match(message, start)
        kind = head.group(1) if head else 'other'
        if kind in CARD_TYPES or full is None:
            items.append({'type': kind if kind in CARD_TYPES else 'other', 'data': {}})
            return items
        data = {}
        for part in full.group(2).split(',')[1:]:
            name, _, value = part.partition('=')
            data[name] = unescape(value)
        items.append({'type': kind, 'data': data})
        at = full.end()
    if at < len(message):
        items.append({'type': 'text', 'data': {'text': unescape(message[at:])}})
    return items


def id_text(value):
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str) and re.fullmatch(r'-?[0-9]+', value.strip()):
        return value.strip()
    return None


def segments(message):
    if isinstance(message, str):
        message = parse_cq(message)
    segs = []
    for item in message if isinstance(message, list) else []:
        if not isinstance(item, dict):
            continue
        kind, data = item.get('type'), item.get('data') if isinstance(item.get('data'), dict) else {}
        if kind == 'text':
            segs.append({'k': 'text', 'text': str(data.get('text', ''))})
        elif kind == 'at':
            seg = {'k': 'at', 'id': 'all' if str(data.get('qq')) == 'all' else id_text(data.get('qq'))}
            if isinstance(data.get('name'), str) and data['name'].strip():
                seg['name'] = data['name'].strip()   # learned by the stripper, never published
            segs.append(seg)
        elif kind == 'reply':
            segs.append({'k': 'reply', 'mid': id_text(data.get('id'))})
        elif kind in ('forward', 'node'):
            segs.append({'k': 'forward'})
        elif kind == 'face':
            segs.append({'k': 'face', 'id': id_text(data.get('id')) or ''})
        else:
            segs.append({'k': 'media', 'kind': MEDIA.get(kind, 'other')})
    return segs


def normalise(row, event):
    message_hash, is_group, session_id, private_direction, timestamp = row
    sender = event.get('sender') if isinstance(event.get('sender'), dict) else {}
    sender_id = id_text(event.get('user_id')) or id_text(sender.get('user_id'))
    if not is_group and private_direction in (0, 1):
        out = private_direction == 1
    else:
        out = event.get('post_type') == 'message_sent' or (sender_id is not None and sender_id == id_text(event.get('self_id')))
    seconds = timestamp if isinstance(timestamp, int) and timestamp > 0 else event.get('time')
    t = int(seconds) * 1000 if isinstance(seconds, (int, float)) and not isinstance(seconds, bool) and seconds > 0 else None
    return {'v': 1, 'src': 'snowluma', 'conv': f"{'group' if is_group else 'private'}:{session_id}",
            'dir': 'out' if out else 'in',
            'sender': {'id': sender_id, 'card': str(sender.get('card') or ''), 'nick': str(sender.get('nickname') or '')}
                      if sender_id else None,
            'convName': str(event['group_name']) if is_group and event.get('group_name') else None,
            't': t, 'mid': id_text(event.get('message_id')) or id_text(message_hash),
            'segs': segments(event.get('message'))}


def _sidecars(path):
    return [path.with_name(path.name + suffix) for suffix in ('-wal', '-shm')]


def marker_path(db):
    return db.with_name(db.name + '.snapshot.json')


def file_sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def seal(db, messages):
    db = Path(db)
    marker_path(db).write_text(json.dumps({'tool': 'snowluma_export.snapshot', 'sha256': file_sha256(db),
                                           'messages': messages}) + '\n', encoding='utf-8')


def sealed(db):
    db = Path(db)
    try:
        marker = json.loads(marker_path(db).read_text(encoding='utf-8'))
        return (isinstance(marker, dict) and marker.get('tool') == 'snowluma_export.snapshot'
                and marker.get('sha256') == file_sha256(db) and not any(p.exists() for p in _sidecars(db)))
    except (OSError, ValueError):
        return False


def _remove_copy(copy):
    for leftover in (copy, *_sidecars(copy), copy.with_name(copy.name + '-journal'), marker_path(copy)):
        try:
            leftover.unlink()
        except FileNotFoundError:
            pass


def snapshot(live, copy):
    live, copy = Path(live).resolve(), Path(copy).resolve()
    live_dir = live.parent
    if copy.exists() or marker_path(copy).exists():
        print('snapshot refused: copy exists')
        return 2
    if Path(os.path.normcase(copy)).is_relative_to(Path(os.path.normcase(live_dir))):
        print('snapshot refused: the copy must be outside the live store directory')
        return 2
    if not all(p.is_file() for p in _sidecars(live)):
        print('snapshot refused: live store has no -wal or -shm beside it')
        return 2
    before = sorted(os.listdir(live_dir))
    src = dst = None
    status, message, count, ckpt = 0, '', 0, 'unavailable'
    try:
        src = sqlite3.connect(live.as_uri() + '?mode=ro', uri=True)
        option = getattr(sqlite3, 'SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE', None)
        if option is not None and hasattr(src, 'setconfig'):
            src.setconfig(option, True)
            ckpt = 'applied'
        src.execute('PRAGMA query_only=1')
        if sorted(os.listdir(live_dir)) != before:
            status, message = 1, 'snapshot failed: opening the live store changed its directory'
        else:
            dst = sqlite3.connect(copy)
            src.backup(dst)
            dst.execute('PRAGMA journal_mode=DELETE')
            count = dst.execute('SELECT COUNT(*) FROM messages').fetchone()[0]
    except sqlite3.Error:
        status, message = 1, 'snapshot failed: the live store could not be read or has no messages table'
    finally:
        if dst is not None:
            dst.close()
        if src is not None:
            src.close()
    if sorted(os.listdir(live_dir)) != before:
        status, message = 1, (message + '; ' if message else '') + 'live directory file set changed during snapshot'
    if not status:
        try:
            seal(copy, count)
        except OSError:
            status, message = 1, 'snapshot failed: the snapshot marker could not be written'
    if status:
        _remove_copy(copy)
        print(message)
        return status
    print(f'snapshot: messages={count} no_ckpt_on_close={ckpt}')
    return 0


def export(db, out, convs):
    out = Path(out)
    if out.exists():
        print('export refused: output exists')
        return 2
    db = Path(db).resolve()
    if not sealed(db):
        print('export refused: --db is not a sealed, unchanged snapshot (run snapshot first)')
        return 2
    try:
        conn = sqlite3.connect(db.as_uri() + '?mode=ro', uri=True)
        try:
            cols = {r[1] for r in conn.execute('PRAGMA table_info(messages)')}
            missing = [name for name in REQUIRED if name not in cols]
            if missing:
                print('export refused: missing columns ' + ', '.join(missing))
                return 2
            rows = conn.execute('SELECT message_hash, is_group, session_id, private_direction, timestamp, data '
                                'FROM messages ORDER BY timestamp, sequence, message_hash').fetchall()
        finally:
            conn.close()
    except sqlite3.Error:
        print('export refused: the snapshot cannot be read')
        return 2
    counts = Counter()
    records = []
    for row in rows:
        counts['rows'] += 1
        if row[5] is None:
            counts['no_data'] += 1
            continue
        try:
            event = json.loads(row[5])
        except json.JSONDecodeError:
            event = None
        if not isinstance(event, dict):
            counts['bad_json'] += 1
            continue
        record = normalise(row[:5], event)
        if convs and record['conv'] not in convs:
            counts['filtered'] += 1
            continue
        records.append(record)
    partial = out.with_name(out.name + '.partial')
    replaced = 0
    try:
        with open(partial, 'w', encoding='utf-8') as handle:
            for record in records:
                line, n = SURROGATE.subn('\ufffd', json.dumps(record, ensure_ascii=False, separators=(',', ':')))
                replaced += n
                handle.write(line + '\n')
        os.replace(partial, out)
    except (OSError, UnicodeError, ValueError):
        try:
            partial.unlink()
        except OSError:
            pass
        print('export failed: the output could not be written')
        return 1
    print(f"export: rows={counts['rows']} written={len(records)} no_data={counts['no_data']} "
          f"bad_json={counts['bad_json']} filtered={counts['filtered']} surrogates_replaced={replaced} "
          f"conversations={len({r['conv'] for r in records})} "
          f"in={sum(r['dir'] == 'in' for r in records)} out={sum(r['dir'] == 'out' for r in records)}")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description='Snapshot and export the SnowLuma message store (#58).')
    commands = parser.add_subparsers(dest='command', required=True)
    take = commands.add_parser('snapshot', help='copy the live store with the online backup API')
    take.add_argument('--live', required=True)
    take.add_argument('--copy', required=True)
    dump = commands.add_parser('export', help='write schema v1 records from a sealed snapshot')
    dump.add_argument('--db', required=True)
    dump.add_argument('--out', required=True)
    dump.add_argument('--conv', action='append', default=[], help='keep only this conversation, group:<id> or private:<id>')
    args = parser.parse_args(argv)
    if args.command == 'snapshot':
        return snapshot(args.live, args.copy)
    return export(args.db, args.out, args.conv)


if __name__ == '__main__':
    sys.exit(main())
