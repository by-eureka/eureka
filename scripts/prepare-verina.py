#!/usr/bin/env python3
"""Prepare Verina proof-generation inputs for the existing harness.

    python3 scripts/prepare-verina.py /path/to/verina runs/verina-batch

Use a pinned reference checkout. Remove the marked proof and proof_aux contents, then apply
small, recorded library compatibility edits for the harness's Lean 4.33 toolchain. Reference
proofs stay outside the templates, which are the only files mounted in prover sandboxes.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
MARKER = re.compile(r'^([ \t]*)-- !benchmark @(start|end) (\w+)\b.*$')


def proof_input(source):
    """Keep the official proof_gen inputs and replace the missing proof with one sorry."""
    output, active, counts = [], None, {'proof': 0, 'proof_aux': 0}
    for line in source.splitlines(keepends=True):
        marker = MARKER.match(line)
        if marker:
            indent, boundary, name = marker.groups()
            if boundary == 'start':
                if active is not None:
                    raise ValueError('Nested benchmark sections are not supported')
                active = name
                output.append(line)
                if name in counts:
                    counts[name] += 1
                if name == 'proof':
                    output.append(indent + 'sorry\n')
            else:
                if active != name:
                    raise ValueError(f'Unexpected section end: {name}')
                active = None
                output.append(line)
        elif active not in ('proof', 'proof_aux'):
            output.append(line)
    if active is not None or counts != {'proof': 1, 'proof_aux': 1}:
        raise ValueError('Expected one closed proof and one proof_aux section')
    result = ''.join(output)
    # The only admission may be the target's new proof hole, never supplied code or a helper.
    if len(re.findall(r'\bsorry\b', result)) != 1:
        raise ValueError('An unexpected sorry remains outside the target proof')
    return result


def compatible_input(source, key):
    """Port old library spelling/inference without changing the program or specification.

    Keep these edits separate from proof stripping so the original benchmark input can be
    audited independently. Every applied replacement is included in problems.json.
    """
    edits = []
    replacements = [('Array.mkArray', 'Array.replicate'),
                    ('Std.HashSet.empty', 'Std.HashSet.emptyWithCapacity')]
    if key == 'verina_basic_61':
        replacements.append(('String.Iterator', 'String.Legacy.Iterator'))
    if key == 'verina_advanced_68':
        replacements.append(('fun c => ¬c.isDigit', 'fun (c : Char) => ¬c.isDigit'))
    if key == 'verina_basic_86':
        # The new compiler's termination-measure search loops here. State the loop's
        # existing countdown explicitly; Lean still checks termination itself.
        replacements.append(('  else b\n-- !benchmark @end code_aux',
                             '  else b\ntermination_by len - i\n-- !benchmark @end code_aux'))
    for old, new in replacements:
        source, count = re.subn(re.escape(old) + r'\b', lambda _: new, source)
        if count:
            edits.append({'old': old, 'new': new, 'count': count})
    return source, edits


def prepare(reference, output):
    if output.exists():
        raise ValueError(f'{output} already exists; use a new directory')
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=reference,
                                       text=True).strip()
    records = []
    files = sorted((reference / 'datasets/verina').glob('*/task.json'),
                   key=lambda p: (p.parent.name.split('_')[1], int(p.parent.name.split('_')[-1])))
    if not files:
        raise ValueError('No Verina task.json files found')
    # Validate every source before creating any output, so an incomplete import cannot launch.
    inputs = []
    for path in files:
        task = json.loads(path.read_text())
        source_path = path.parent / task['lean_file']
        raw = source_path.read_text()
        benchmark_input = proof_input(raw)
        text, edits = compatible_input(benchmark_input, task['id'])
        names = re.findall(r'^theorem\s+([^\s(:]+)', text, re.M)
        expected = task['signature']['name'] + '_spec_satisfied'
        if not names or names[-1] != expected:
            raise ValueError(f'{task["id"]}: target theorem does not match the task signature')
        inputs.append((task, path.parent, raw, benchmark_input, text, edits, expected))
    output.mkdir(parents=True)
    (output / 'prompts').mkdir()
    for number, (task, directory, raw, benchmark_input, text, edits, name) in enumerate(inputs, 1):
        key = task['id']
        template = output / 'templates' / key
        (template / 'Proofs').mkdir(parents=True)
        for filename in ('lakefile.toml', 'lake-manifest.json', 'lean-toolchain', 'Proofs.lean'):
            shutil.copy2(ROOT / 'examples/simple' / filename, template / filename)
        for filename in ('Challenge.lean', 'Solution.lean'):
            (template / 'Proofs' / filename).write_text(text)
        supplied_axioms = re.findall(r'^\s*axiom\s+([^\s({:]+)', text, re.M)
        config = {'challenge_module': 'Proofs.Challenge', 'solution_module': 'Proofs.Solution',
                  'theorem_names': [name], 'permitted_axioms': [
                      'propext', 'Quot.sound', 'Classical.choice', *supplied_axioms]}
        (template / 'config.json').write_text(json.dumps(config, indent=2) + '\n')
        description = (directory / task['description_file']).read_text().strip()
        prompt = ('Prove the correctness theorem in Proofs/Solution.lean. The supplied program, '
                  'precondition, postcondition and their helper declarations are trusted inputs; '
                  'preserve them and the theorem statement. Add proof lemmas if useful and fill '
                  'the proof hole. Work from the supplied input and mathematical libraries. '
                  'Do not look up Verina reference solutions. Use the Lean tools to check your '
                  'work and submit only a complete proof.\n\nTask description:\n' + description)
        prompt_file = output / 'prompts' / (key + '.txt')
        prompt_file.write_text(prompt + '\n')
        records.append({'key': key, 'question_id': number, 'task_id': key,
            'split': key.split('_')[1], 'variant': 'proof_gen', 'theorem_name': name,
            'source_revision': revision, 'source_path': str(directory.relative_to(reference)),
            'source_sha256': hashlib.sha256(raw.encode()).hexdigest(),
            'benchmark_input_sha256': hashlib.sha256(benchmark_input.encode()).hexdigest(),
            'input_sha256': hashlib.sha256(text.encode()).hexdigest(),
            'compatibility_edits': edits,
            'supplied_axioms': supplied_axioms, 'template': str(template),
            'prompt_file': str(prompt_file), 'run': str(output / 'problems' / key)})
    (output / 'problems.json').write_text(json.dumps(records, indent=2) + '\n')
    return records


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('reference', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    records = prepare(args.reference.resolve(), args.output.resolve())
    print(f'Prepared {len(records)} proof-generation tasks in {args.output}')
