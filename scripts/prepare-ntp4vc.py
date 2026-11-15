#!/usr/bin/env python3
"""Prepare the 600 Lean NTP4VC verification conditions, without solving any goals.

First use --support-only to create an image build context. After compiling the original
stubs and extracting their trusted axiom policies, use --policies FILE to write templates.
The reference checkout and benchmark targets never enter the support image.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
IMPORT = re.compile(r'^import\s+(\S+)\s*$', re.M)
LOGIC = ['propext', 'Quot.sound', 'Classical.choice']


def inventory(reference):
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=reference,
                                       text=True).strip()
    paths = [p.strip() for p in (reference / 'test_set.lean.txt.lst').read_text().splitlines()
             if p.strip()]
    if len(paths) != 600 or len(set(paths)) != 600:
        raise ValueError('Expected 600 distinct tasks in the official Lean manifest')
    targets = set(paths)
    support, records = {}, []

    def visit(module):
        if module == 'Mathlib' or module.startswith(('Mathlib.', 'Lean.', 'Std.', 'Init.')):
            return
        if module in support:
            return
        if module.startswith('Why3.'):
            path = 'generation/lean/' + module.replace('.', '/') + '.lean'
        elif module.startswith(('frama_c.', 'pearl.')):
            path = 'data/why3/' + module.replace('.', '/') + '.lean'
        else:
            raise ValueError(f'Unknown benchmark import: {module}')
        if path in targets:
            raise ValueError(f'A benchmark target cannot be a supplied dependency: {path}')
        source = (reference / path).read_text()
        if re.search(r'\b(sorry|admit)\b', source):
            raise ValueError(f'Admission in support module: {path}')
        support[module] = (path, source)
        for imported in IMPORT.findall(source):
            visit(imported)

    for number, path in enumerate(paths, 1):
        source = (reference / path).read_text()
        namespaces = re.findall(r'^namespace\s+(\S+)', source, re.M)
        names = re.findall(r'^theorem\s+([^\s(:]+)', source, re.M)
        if len(namespaces) != 1 or len(names) != 1 or len(re.findall(r'\bsorry\b', source)) != 1:
            raise ValueError(f'Expected one namespaced target and one proof hole: {path}')
        for imported in IMPORT.findall(source):
            visit(imported)
        records.append({'key': f'NTP{number:03d}', 'question_id': number,
                        'source_path': path, 'source_revision': revision,
                        'category': path.split('/')[2],
                        'theorem_name': namespaces[0] + '.' + names[0],
                        'input_sha256': hashlib.sha256(source.encode()).hexdigest()})
    return records, support


def support_image(reference, output):
    records, support = inventory(reference)
    build = output / 'image'
    build.mkdir(parents=True, exist_ok=False)
    package = build / 'ntpSupport'
    for module, (path, source) in support.items():
        if module == 'Why3.Base':
            source = source.replace('List.Sorted', 'List.Pairwise')
        target = package / (module.replace('.', '/') + '.lean')
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(source)
    for name in ('Why3', 'frama_c', 'pearl'):
        (package / (name + '.lean')).write_text('-- Modules are imported separately by each task.\n')
    options = ('\n[leanOptions]\nautoImplicit = false\nrelaxedAutoImplicit = false\n'
               'maxSynthPendingDepth = 3\n')
    base = (ROOT / 'examples/simple/lakefile.toml').read_text()
    support_lake = (base.replace('name = "beam_mathlib"', 'name = "ntpSupport"')
                   .replace('name = "Proofs"', 'name = "Why3"'))
    support_lake += options + '\n[[lean_lib]]\nname = "frama_c"\n\n[[lean_lib]]\nname = "pearl"\n'
    (package / 'lakefile.toml').write_text(support_lake)
    shutil.copy2(ROOT / 'examples/simple/lean-toolchain', package / 'lean-toolchain')
    project = build / 'project'
    project.mkdir()
    (project / 'lakefile.toml').write_text(base + options + '\n[[require]]\nname = "ntpSupport"\n'
        'path = ".lake/packages/ntpSupport"\n')
    for name in ('lean-toolchain', 'lake-manifest.json'):
        shutil.copy2(ROOT / 'examples/simple' / name, project / name)
    (build / 'modules.json').write_text(json.dumps(list(support)))
    (output / 'preparation.json').write_text(json.dumps({
        'source_revision': records[0]['source_revision'], 'tasks': len(records),
        'support_modules': len(support), 'compatibility_edits': [
            {'file': 'Why3/Base.lean', 'old': 'List.Sorted', 'new': 'List.Pairwise'}]}, indent=2) + '\n')


def templates(reference, output, policies):
    records, _ = inventory(reference)
    allowed = {row['module'].rsplit('.', 1)[-1]: row['permitted_axioms']
               for row in map(json.loads, policies.read_text().splitlines())}
    if set(allowed) != {r['key'] for r in records}:
        raise ValueError('A compiled axiom policy is required for every original input')
    if (output / 'templates').exists():
        raise ValueError('Templates already exist; use a new batch')
    project = output / 'prepared-project'
    for record in records:
        key = record['key']
        axioms = allowed[key]
        if 'sorryAx' in axioms or any('_native.' in name for name in axioms):
            raise ValueError(f'{key}: an admission/native assertion is not a benchmark axiom')
        template = output / 'templates' / key
        (template / 'Proofs').mkdir(parents=True)
        for name in ('lakefile.toml', 'lake-manifest.json', 'lean-toolchain'):
            shutil.copy2(project / name, template / name)
        shutil.copy2(ROOT / 'examples/simple/Proofs.lean', template / 'Proofs.lean')
        source = (reference / record['source_path']).read_bytes()
        for name in ('Challenge.lean', 'Solution.lean'):
            (template / 'Proofs' / name).write_bytes(source)
        config = {'challenge_module': 'Proofs.Challenge', 'solution_module': 'Proofs.Solution',
                  'theorem_names': [record['theorem_name']], 'permitted_axioms': axioms}
        (template / 'config.json').write_text(json.dumps(config, indent=2) + '\n')
        prompt = ('Prove the verification condition in Proofs/Solution.lean. Preserve the supplied '
                  'definitions, axioms, theorem statement and imports. Fill the proof hole; add '
                  'proof lemmas if useful. Use the Lean tools and submit a complete proof. '
                  'Work from the supplied input and mathematical libraries. Do not look up '
                  'benchmark reference solutions. Proofs must pass the bundled Lean verifier. native_decide, '
                  'sorry and additional axioms are not permitted. For local search use a '
                  'declaration-name fragment; for Loogle use qualified constants or quoted '
                  'name substrings. Use actual zero-based file positions with lean_todo. '
                  'Use lean_sync or lake build Proofs.Solution to check the target file.\n')
        prompt_file = output / 'prompts' / (key + '.txt')
        prompt_file.parent.mkdir(exist_ok=True)
        prompt_file.write_text(prompt)
        record.update(template=str(template), prompt_file=str(prompt_file),
                      run=str(output / 'problems' / key), variant='proof_gen',
                      supplied_axioms=[a for a in axioms if a not in LOGIC])
    (output / 'problems.json').write_text(json.dumps(records, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('reference', type=Path)
    parser.add_argument('output', type=Path)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--support-only', action='store_true')
    mode.add_argument('--policies', type=Path)
    args = parser.parse_args()
    reference, output = args.reference.resolve(), args.output.resolve()
    if args.support_only:
        support_image(reference, output)
    else:
        templates(reference, output, args.policies)
    print(f'Prepared NTP4VC {"support image context" if args.support_only else "600 templates"}: {output}')
