import importlib.util
import io
from pathlib import Path
import sys
import time
import tomllib
import unittest
import wave

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import call_test as ct  # noqa: E402

INSTALLED = '''machines = ["A", "B"]

[imessage]
recipient = "+61400000000"
handle_ids = [1, 36]

[store]
path = "/Users/someone/bin/harold/data/events-v3"

[tts]
command = "/venv/bin/python"
args = ["-m", "mlx_audio.tts.generate", "--play", "--text"]
fallback_command = "say"

[notify]
chief_session = "chief"

[[timed_prompts]]
name = "digest"
at = "07:50"
folder = "/chief"
text = "digest"

[web_app]
listen = "127.0.0.1:4330"
allowed_login = "someone@example.com"

[voice_input]
command = "/venv/bin/python"
args = ["-m", "mlx_audio.stt.generate", "--model", "mlx-community/whisper-large-v3-turbo"]
'''


def scratch_config():
    return tomllib.loads(ct.scratch_config(
        INSTALLED,
        chief_dir='/scratch/chief',
        grpc_port=50111,
        web_port=4411,
        store_path='/scratch/store',
        chat_db_path='/scratch/no-messages.db',
    ))


class ScratchConfigTests(unittest.TestCase):
    def test_sets_the_chief_folder_the_session_and_the_ports(self):
        config = scratch_config()
        self.assertEqual('/scratch/chief', config['notify']['chief'])
        self.assertEqual('harold-call-test', config['notify']['chief_session'])
        self.assertEqual(['/scratch/chief'], config['notify']['web_app'])
        self.assertEqual(50111, config['grpc']['port'])
        self.assertEqual('127.0.0.1:4411', config['web_app']['listen'])
        self.assertEqual('call-test', config['web_app']['allowed_login'])
        self.assertIs(False, config['web_app']['require_pairing'])
        self.assertEqual('/scratch/store', config['store']['path'])
        self.assertIs(False, config['telemetry']['export'])

    def test_copies_voice_input_and_tts_from_the_installed_config(self):
        config = scratch_config()
        installed = tomllib.loads(INSTALLED)
        self.assertEqual(installed['voice_input'], config['voice_input'])
        self.assertEqual(installed['tts'], config['tts'])

    def test_holds_only_a_placeholder_imessage_recipient_and_no_timed_prompts(self):
        config = scratch_config()
        self.assertEqual({'recipient': 'call-test@harold.invalid', 'handle_ids': [0]}, config['imessage'])
        self.assertNotIn('timed_prompts', config)
        self.assertNotIn('machines', config)
        self.assertNotIn('alert_listen', config['telemetry'])
        self.assertNotIn('pod_registry', config['telemetry'])

    def test_turns_off_the_channels_that_default_on_and_the_messages_database(self):
        config = scratch_config()
        self.assertEqual([], config['notify']['imessage'])
        self.assertEqual([], config['notify']['machine_audio_output'])
        self.assertEqual('/scratch/no-messages.db', config['chat_db']['path'])

    def test_copying_stops_at_the_next_table(self):
        text = ct.table_text(INSTALLED, 'tts')
        self.assertIn('fallback_command = "say"', text)
        self.assertNotIn('chief_session', text)
        self.assertEqual('', ct.table_text(INSTALLED, 'missing'))


class PiecesTests(unittest.TestCase):
    def test_splits_into_4000_sample_pieces_stamped_every_250_ms(self):
        pcm = bytes(range(256)) * 78 + b'\x01\x02' * 10  # 19,988 bytes: 9,994 samples
        pieces = ct.pieces(pcm)
        self.assertEqual([0, 250, 500], [at for at, _ in pieces])
        self.assertEqual([8000, 8000, 3988], [len(body) for _, body in pieces])
        self.assertEqual(pcm, b''.join(body for _, body in pieces))

    def test_stamps_start_at_the_first_piece_offset(self):
        pieces = ct.pieces(b'\0\0' * 8000, first_at_ms=1000)
        self.assertEqual([1000, 1250], [at for at, _ in pieces])

    def test_phone_audio_is_one_second_of_silence_then_the_question_then_two(self):
        question = b'\x05\x00' * 16000
        pcm = ct.phone_audio(question)
        self.assertEqual(2 * 16000 * (1 + 1 + 2), len(pcm))
        self.assertEqual(b'\0' * 32000, pcm[:32000])
        self.assertEqual(question, pcm[32000:64000])
        self.assertEqual(b'\0' * 64000, pcm[64000:])


class WordMatchTests(unittest.TestCase):
    QUESTION = 'what is the status of the build today'

    def test_passes_at_eighty_percent(self):
        # 8 words in the question; 7 of 8 is 87.5%, 6.4 needed.
        self.assertTrue(ct.heard_enough('What is the status of the build', self.QUESTION))

    def test_passes_exactly_at_the_threshold(self):
        self.assertTrue(ct.heard_enough('one two three four', 'one two three four five'))

    def test_fails_below(self):
        self.assertFalse(ct.heard_enough('one two three', 'one two three four five'))

    def test_ignores_case_and_punctuation(self):
        self.assertEqual(1.0, ct.word_match('"Hello, World!"', 'hello world'))

    def test_nothing_heard_matches_nothing(self):
        self.assertEqual(0.0, ct.word_match('', self.QUESTION))


class StepTests(unittest.TestCase):
    def test_a_step_that_finishes_in_time_passes_with_its_time_and_detail(self):
        result = ct.run_step('POST /call', 1.0, lambda: 'id abc')
        self.assertTrue(result.ok)
        self.assertEqual('POST /call', result.name)
        self.assertEqual('id abc', result.detail)
        self.assertGreaterEqual(result.ms, 0)

    def test_a_step_past_its_limit_fails_and_names_itself(self):
        result = ct.run_step('turn is cut', 0.05, lambda: time.sleep(0.15))
        self.assertFalse(result.ok)
        self.assertIn('turn is cut', result.detail)
        self.assertIn('limit', result.detail)

    def test_a_step_that_raises_fails_with_the_reason(self):
        def broken():
            raise ct.StepFailed('answered 409')

        result = ct.run_step('POST /call', 1.0, broken)
        self.assertFalse(result.ok)
        self.assertEqual('answered 409', result.detail)

    def test_wait_until_raises_when_the_limit_passes(self):
        with self.assertRaises(ct.StepFailed) as raised:
            ct.wait_until(lambda: None, 0.05, 'the sound names an answer', every=0.01)
        self.assertIn('the sound names an answer', str(raised.exception))

    def test_wait_until_returns_the_first_truthy_value(self):
        values = iter([None, '', 'ready'])
        self.assertEqual('ready', ct.wait_until(lambda: next(values), 1.0, 'x', every=0.001))


class ReportTests(unittest.TestCase):
    def test_every_step_passing_prints_a_line_each_and_exits_0(self):
        results = [ct.StepResult('a', True, 12, 'ok'), ct.StepResult('b', True, 3400, '')]
        lines, code = ct.report(results)
        self.assertEqual(0, code)
        self.assertEqual(2, len(lines))
        self.assertIn('a', lines[0])
        self.assertIn('12 ms', lines[0])
        self.assertIn('3400 ms', lines[1])

    def test_a_failing_step_is_named_and_exits_1(self):
        results = [ct.StepResult('a', True, 5, ''), ct.StepResult('b', False, 900, 'b took too long')]
        lines, code = ct.report(results)
        self.assertEqual(1, code)
        self.assertTrue(any('FAIL' in line and 'b' in line for line in lines))


class CallScreenTests(unittest.TestCase):
    def test_reads_the_state_the_sound_and_what_was_heard(self):
        patch = ('<section id="call" class="call" data-harold-state="thinking" data-harold-sound="abc" '
                 'data-harold-speech="">\n<p class="call-heard">You said:<br><span>"🎙 what&#39;s up"</span></p>')
        self.assertEqual(
            {'state': 'thinking', 'sound': 'abc', 'heard': "what's up", 'line': ''}, ct.parse_call(patch))

    def test_a_patch_without_the_call_screen_is_none(self):
        self.assertIsNone(ct.parse_call('<div id="messages"></div>'))

    def test_the_call_ending_in_the_log(self):
        self.assertTrue(ct.call_ended('INFO app call ended reason="stopped"', 'stopped'))
        self.assertFalse(ct.call_ended('INFO app call ended reason="bye"', 'stopped'))


class AnswerTests(unittest.TestCase):
    def test_a_tick_or_a_phrase_is_not_the_answer(self):
        tick = {'state': 'thinking', 'sound': 'abc', 'heard': '', 'line': ''}
        phrase = {'state': 'speaking', 'sound': 'abc', 'heard': '', 'line': "The chief is busy."}
        self.assertFalse(ct.is_answer(tick))
        self.assertFalse(ct.is_answer(phrase))

    def test_the_chiefs_line_with_a_sound_is_the_answer(self):
        answer = {'state': 'speaking', 'sound': 'abc', 'heard': '', 'line': ct.ANSWER_LINE}
        self.assertTrue(ct.is_answer(answer))
        self.assertFalse(ct.is_answer(dict(answer, sound='')))


class PaneTests(unittest.TestCase):
    def test_spoken_prompt_lines_name_the_pane(self):
        log = (
            '2026 INFO harold: spoken prompt typed pane_id=%7 words=8\n'
            '\x1b[2m2026\x1b[0m INFO harold: spoken prompt typed pane_id=%9 words=3\n'
            'other line\n'
        )
        self.assertEqual(['%7', '%9'], ct.typed_panes(log))
        self.assertEqual([], ct.typed_panes('nothing'))



class SpeechModelRecordTests(unittest.TestCase):
    def make_cache(self, root, model, commit, files):
        snapshot = Path(root) / ('models--' + model.replace('/', '--')) / 'snapshots' / commit
        blobs = Path(root) / ('models--' + model.replace('/', '--')) / 'blobs'
        snapshot.mkdir(parents=True)
        blobs.mkdir()
        for name, size in files.items():
            blob = blobs / ('b-' + name)
            blob.write_bytes(b'x' * size)
            (snapshot / name).symlink_to(blob)
        return snapshot

    def test_the_cache_is_hub_cache_then_hf_home_then_the_home_cache(self):
        self.assertEqual(Path('/c'), ct.hub_cache({'HF_HUB_CACHE': '/c', 'HF_HOME': '/h', 'HOME': '/u'}))
        self.assertEqual(Path('/h/hub'), ct.hub_cache({'HF_HOME': '/h', 'HOME': '/u'}))
        self.assertEqual(Path('/u/.cache/huggingface/hub'), ct.hub_cache({'HOME': '/u'}))

    def test_the_models_are_the_installed_whisper_and_the_default_kokoro_at_the_pinned_commits(self):
        self.assertEqual(
            [(ct.WHISPER_MODEL, ct.WHISPER_REVISION), (ct.KOKORO_MODEL, ct.KOKORO_REVISION)],
            ct.configured_models(INSTALLED))
        other = INSTALLED.replace('whisper-large-v3-turbo', 'whisper-small') + '\n'
        self.assertEqual('mlx-community/whisper-small', ct.configured_models(other)[0][0])

    def test_a_record_lists_each_cached_file_with_its_size_and_changes_nothing(self):
        import tempfile
        with tempfile.TemporaryDirectory() as root:
            snapshot = self.make_cache(root, ct.WHISPER_MODEL, ct.WHISPER_REVISION,
                                       {'weights.safetensors': 100, 'config.json': 7})
            before = sorted(str(p) for p in Path(root).rglob('*'))
            records = ct.speech_model_records(INSTALLED, Path(root))
            self.assertEqual(before, sorted(str(p) for p in Path(root).rglob('*')))
        name = 'mlx-community--whisper-large-v3-turbo@{}.json'.format(ct.WHISPER_REVISION)
        self.assertEqual([name], list(records))
        record = __import__('json').loads(records[name])
        self.assertEqual(ct.WHISPER_MODEL, record['model'])
        self.assertEqual(ct.WHISPER_REVISION, record['commit'])
        self.assertEqual(
            {('config.json', 7), ('weights.safetensors', 100)},
            {(f['path'], f['size']) for f in record['files']})
        self.assertTrue(snapshot.name)

    def test_a_model_the_cache_lacks_gets_no_record(self):
        import tempfile
        with tempfile.TemporaryDirectory() as root:
            self.assertEqual({}, ct.speech_model_records(INSTALLED, Path(root)))


class TurnTimeTests(unittest.TestCase):
    def phone(self, events):
        phone = ct.Phone(1)
        phone.events = events
        return phone

    def test_first_sound_and_answer_count_from_the_end_of_the_speech(self):
        shown = lambda sound, line: {'state': 'speaking', 'sound': sound, 'heard': '', 'line': line}
        phone = self.phone([
            (9.0, shown('t0', '')),
            (11.0, shown('c1', 'Wait.')),
            (12.0, shown('t1', '')),
            (14.5, shown('a1', 'One todo is waiting.')),
        ])
        results = ct.turn_times('todos', phone, 0, 10.0, lambda line: 'todo' in line)
        self.assertEqual(
            [('todos: first_sound_ms', True, 1000), ('todos: answer_ms', True, 4500)],
            [(r.name, r.ok, r.ms) for r in results])

    def test_a_sound_with_no_words_is_no_first_sound(self):
        shown = {'state': 'speaking', 'sound': 't', 'heard': '', 'line': ''}
        results = ct.turn_times('x', self.phone([(11.0, shown)]), 0, 10.0, lambda line: True)
        self.assertFalse(any(r.ok for r in results))


class WorkerReadyTests(unittest.TestCase):
    def test_reads_the_load_time_and_who_started_the_worker(self):
        log = '2026 INFO harold: voice worker ready load_ms=3100 started_by="page"\n'
        self.assertEqual('load_ms=3100 started_by=page', ct.worker_ready_line(log))
        self.assertEqual('', ct.worker_ready_line('nothing'))


def wav_of(seconds, rate=24000):
    out = io.BytesIO()
    with wave.open(out, 'wb') as sound:
        sound.setnchannels(1)
        sound.setsampwidth(2)
        sound.setframerate(rate)
        sound.writeframes(b'\0\0' * round(seconds * rate))
    return out.getvalue()


class SoundNameTests(unittest.TestCase):
    def test_a_sound_is_named_by_its_length(self):
        self.assertEqual('ringback', ct.sound_name(wav_of(3.0)))
        self.assertEqual('pickup', ct.sound_name(wav_of(0.15)))
        self.assertEqual('connecting', ct.sound_name(wav_of(0.6)))
        self.assertEqual('tick', ct.sound_name(wav_of(0.04)))
        self.assertEqual('sound', ct.sound_name(wav_of(1.7)))

    def test_bytes_that_are_not_a_wav_are_a_sound(self):
        self.assertEqual('sound', ct.sound_name(b'not a wav'))

    def test_a_run_of_the_same_sound_is_counted(self):
        self.assertEqual('ringback x2, pickup', ct.sound_summary(['ringback', 'ringback', 'pickup']))
        self.assertEqual('ringback, pickup', ct.sound_summary(['ringback', 'pickup']))
        self.assertEqual('none', ct.sound_summary([]))


class OneMomentTests(unittest.TestCase):
    def shown(self, sound, line):
        return {'state': 'speaking', 'sound': sound, 'line': line, 'heard': ''}

    def test_counts_the_sounds_that_said_it(self):
        events = [(0.0, self.shown('a', ct.ONE_MOMENT)), (0.1, self.shown('a', ct.ONE_MOMENT)),
                  (0.2, self.shown('b', 'Fine.')), (0.3, self.shown('c', ct.ONE_MOMENT))]
        self.assertEqual(2, ct.one_moment_sounds(events))

    def test_none_when_the_answer_is_the_first_sound(self):
        self.assertEqual(0, ct.one_moment_sounds([(0.0, self.shown('a', 'Fine.'))]))


if __name__ == '__main__':
    unittest.main()
