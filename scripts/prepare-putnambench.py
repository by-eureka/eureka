#!/usr/bin/env python3
"""Prepare PutnamBench's answers-supplied setting for scripts/run.sh.

    python3 scripts/prepare-putnambench.py runs/putnambench-upstream runs/putnambench

Use a pinned upstream checkout. Answer placeholders are filled from the official
answer comments. The theorem and its proof hole stay unchanged. The bundled Lean verifier treats
these answers as fixed definitions, so a candidate cannot redefine the question.
Only generated templates enter agent sandboxes; the reference checkout stays out.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
ANSWER = re.compile(r'^(?:noncomputable )?abbrev (\w+)\s*:.*:=\s*sorry\s*$')


def proof_input(source, name):
    """Fill only the documented answer hole; never replace a theorem proof."""
    lines = source.splitlines(keepends=True)
    answers = []
    for i, line in enumerate(lines):
        match = ANSWER.fullmatch(line.rstrip('\n'))
        if not match:
            continue
        if i + 1 == len(lines) or not lines[i + 1].startswith('--'):
            raise ValueError(f'{name}: missing official answer comment')
        answer = lines[i + 1][2:].strip()
        if not answer or re.search(r'\b(sorry|admit)\b', answer):
            raise ValueError(f'{name}: incomplete official answer')
        # Some numerical answers use noncomputable real operations. The package's
        # reference rewrite inlines them into the theorem; here we retain the name.
        replacement = line[:line.rfind('sorry')] + answer + '\n'
        if not replacement.startswith('noncomputable '):
            replacement = 'noncomputable ' + replacement
        lines[i] = replacement
        answers.append({'name': match[1], 'value': answer})
    text = ''.join(lines)
    names = re.findall(r'^theorem\s+([^\s(:]+)', text, re.M)
    if names != [name] or len(re.findall(r'\bsorry\b', text)) != 1:
        raise ValueError(f'{name}: expected exactly one theorem proof hole')
    if re.search(r'^\s*axiom\s', text, re.M):
        raise ValueError(f'{name}: unexpected supplied axiom')
    return text, answers


def prepare(reference, output):
    if output.exists():
        raise ValueError(f'{output} already exists; use a new directory')
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=reference,
                                       text=True).strip()
    inputs = []
    for path in sorted((reference / 'lean4/src').glob('putnam_*.lean')):
        raw = path.read_text()
        text, answers = proof_input(raw, path.stem)
        if not text.startswith('import Mathlib\n'):
            raise ValueError(f'{path.stem}: unexpected import header')
        # Keep the prepared pool's Lake configuration identical. This local
        # option supplies upstream's package setting without changing the snapshot.
        text = text.replace('import Mathlib\n',
                            'import Mathlib\n\nset_option autoImplicit false\n', 1)
        inputs.append((path, raw, text, answers))
    if not inputs:
        raise ValueError('No PutnamBench Lean problems found')
    output.mkdir(parents=True)
    (output / 'prompts').mkdir()
    records = []
    for number, (path, raw, text, answers) in enumerate(inputs, 1):
        key = path.stem
        template = output / 'templates' / key
        (template / 'Proofs').mkdir(parents=True)
        for filename in ('lakefile.toml', 'lake-manifest.json', 'lean-toolchain', 'Proofs.lean'):
            shutil.copy2(ROOT / 'examples/simple' / filename, template / filename)
        for filename in ('Challenge.lean', 'Solution.lean'):
            (template / 'Proofs' / filename).write_text(text)
        config = {'challenge_module': 'Proofs.Challenge', 'solution_module': 'Proofs.Solution',
                  'theorem_names': [key], 'permitted_axioms': [
                      'propext', 'Quot.sound', 'Classical.choice']}
        (template / 'config.json').write_text(json.dumps(config, indent=2) + '\n')
        prompt = ('Prove the theorem in Proofs/Solution.lean. This is PutnamBench with '
                  'the official answers supplied. Preserve the statement and supplied '
                  'definitions, including answer definitions. Fill the proof hole and add '
                  'proof lemmas if useful. Use the Lean tools to check the proof, then call '
                  'submit. A proof is complete only after submit verifies it. Work from the '
                  'input and mathematical libraries; do not look up benchmark solutions. '
                  'Library source under .lake/packages is available through the sandbox '
                  'shell. Line numbers in the file tools start at 0.\n')
        prompt_file = output / 'prompts' / (key + '.txt')
        prompt_file.write_text(prompt)
        records.append({'key': key, 'question_id': number, 'theorem_name': key,
            'variant': 'with_answer', 'source_revision': revision,
            'source_path': str(path.relative_to(reference)),
            'source_sha256': hashlib.sha256(raw.encode()).hexdigest(),
            'input_sha256': hashlib.sha256(text.encode()).hexdigest(),
            'official_answers': answers, 'template': str(template),
            'prompt_file': str(prompt_file), 'run': str(output / 'problems' / key)})
    (output / 'problems.json').write_text(json.dumps(records, indent=2) + '\n')
    shutil.copy2(reference / 'lean4/LICENSE', output / 'LICENSE')
    return records


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('reference', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    records = prepare(args.reference.resolve(), args.output.resolve())
    print(f'Prepared {len(records)} PutnamBench inputs in {args.output}')
