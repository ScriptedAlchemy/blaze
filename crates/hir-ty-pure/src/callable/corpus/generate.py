#!/usr/bin/env python3
"""Translate sealed fixture inputs; never execute the oracle or a graph solver.

One strict JSON request (inline, a file, or -): {schema, mode, output}.
Run from callable/: python3 -B corpus/generate.py '<request>'.
The output is an explicit file outside corpus/; check mode performs no writes.
"""
from __future__ import annotations

import ast
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import sys

BUNDLE_SHA256 = '54a1c991ee0ed1574fdeae7157a47e6f7ce814277b527c7024bc9a821e99682c'
MANIFEST_SHA256 = '7fa3b059430ab4c8f8e4e1eb06771a16ba273d19bb493ddcb63f187e935b9375'
CORPUS_SHA256 = 'a1732cb42544473df36b7b093b7a2d60aeac2ad9ff49fdd6189e5094c32b4fde'
RESULT_SHA256 = '381cad2cd66ad38780a0bee02f1325d197eaba233efbb065ba0ac162544c22ba'
ORACLE_SHA256 = 'd97367497a56777eb112bfa111e429e031ba9595867f1e371a629467ade05352'
EFFECT_BITS = {'io.write': 0, 'unwind': 1}
POSITIVE = frozenset({
    'byte_full_token_alias', 'dormant_function_body', 'heap_callback_through_helpers',
    'live_slot_overwrite', 'mixed_branch_effectful', 'mixed_branch_pure',
    'pure_typed_trampoline', 'recursive_finite', 'returned_callback_factory',
    'union_full_token_alias', 'unused_effectful_fn_creation',
})
EXPOSURE_OPEN = frozenset({
    'exposed_scalar_reaches_live_slot', 'unconstrained_integer_reaches_live_slot',
    'external_raw_memory_callback',
})
FAILED_OPEN = frozenset({
    'arity_mismatch', 'invalid_address', 'memory_address_is_not_code',
    'partial_bytes_refused', 'uninitialized_cell',
})


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def encoded(value):
    return (json.dumps(value, indent=2, sort_keys=True) + '\n').encode()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate JSON field: ' + key)
        result[key] = value
    return result


def load_json(data):
    return json.loads(data, object_pairs_hook=unique_object)


@dataclass(frozen=True)
class Request:
    schema: str
    mode: str
    output: str

    @classmethod
    def parse(cls, value):
        require(isinstance(value, dict), 'request must be an object')
        require(set(value) == {'schema', 'mode', 'output'}, 'request fields mismatch')
        require(all(isinstance(v, str) for v in value.values()), 'request fields must be strings')
        result = cls(**value)
        require(result.schema == 'callable-fixture-source/1', 'request schema mismatch')
        require(result.mode in {'generate', 'check'}, 'mode must be generate or check')
        require(bool(result.output.strip()), 'output file must be explicit')
        return result


def read_sealed():
    raw = Path(__file__).with_name('inputs.json').read_bytes()
    require(digest(raw) == BUNDLE_SHA256, 'sealed inputs.json identity mismatch')
    bundle = load_json(raw)
    require(set(bundle) == {'schema', 'original', 'files'}, 'input bundle fields mismatch')
    require(bundle['schema'] == 'callable-fixture-inputs/1', 'input bundle schema mismatch')
    manifest = bundle['original']['manifest'].encode()
    require(digest(manifest) == MANIFEST_SHA256, 'original manifest identity mismatch')
    seals = {'SHA256SUMS': MANIFEST_SHA256}
    for line in manifest.decode().splitlines():
        expected, relative = line.split('  ', 1)
        path = Path(relative)
        require(not path.is_absolute() and '..' not in path.parts, 'unsafe sealed path')
        require(relative not in seals, 'duplicate sealed path: ' + relative)
        seals[relative] = expected
    for relative, value in bundle['files'].items():
        require(relative in seals, 'unsealed bundled file: ' + relative)
        require(digest(encoded(value)) == seals[relative], 'sealed input changed: ' + relative)
    require(seals['corpus.json'] == CORPUS_SHA256, 'corpus identity mismatch')
    require(seals['self-test-result.json'] == RESULT_SHA256, 'recorded result identity mismatch')
    require(seals['interpreter.py'] == ORACLE_SHA256, 'original oracle identity mismatch')
    return bundle, seals


class Adapter:
    """Translate input syntax into graph edges; no evaluation, reachability or fixpoint."""
    def __init__(self, analyzer):
        require(analyzer['schema'] == 'callable-analyzer-input/1', 'analyzer schema mismatch')
        self.analyzer = analyzer
        self.program = analyzer['program']
        require(self.program['schema'] == 'concrete-callable-program/1', 'program schema mismatch')
        self.functions = sorted(self.program['functions'])
        self.function_ids = {name: i for i, name in enumerate(self.functions)}
        self.cells = sorted(self.program['cells'])
        self.cell_ids = {name: i for i, name in enumerate(self.cells)}
        self.root = self.function_ids[self.program['entry']]
        self.slots = {}
        self.returns = {}
        self.cell_nodes = {}
        self.count = 0
        self.edges = []
        self.unsupported = []
        self.bodies = []
        for name in self.functions:
            body = self.program['functions'][name]
            for param in body['params']:
                self.slots[name, param] = self.node()
            self.returns[name] = self.node()
            labels = {ins[1] for ins in body['body'] if ins[0] == 'effect'}
            mask = sum(1 << EFFECT_BITS[label] for label in labels)
            self.bodies.append({'name': name, 'arity': len(body['params']), 'effects': mask})
        for name in self.cells:
            self.cell_nodes[name] = self.node()

    def node(self):
        result = self.count
        self.count += 1
        return result

    def slot(self, owner, name):
        if name is None:
            return self.node()
        if (owner, name) not in self.slots:
            self.slots[owner, name] = self.node()
        return self.slots[owner, name]

    def edge(self, owner, kind, *args):
        self.edges.append((owner, kind, *args))

    def descriptor(self, owner, into, descriptor):
        kind = descriptor['kind']
        if kind == 'uninitialized':
            return  # Bottom storage, not Opaque: stores may initialize it before a load.
        if kind == 'opaque':
            atom = ('Opaque',)
        elif kind == 'known_token':
            token = descriptor['token']
            require(token['kind'] in {'fn', 'addr', 'scalar', 'exposed'}, 'unknown token kind')
            if token['kind'] == 'fn':
                atom = ('Function', self.function_ids[token['payload']])
            elif token['kind'] == 'addr':
                atom = ('Address', self.cell_ids[token['payload']])
            else:
                atom = ('Opaque',)  # Never identify a cell by a concrete integer witness.
        else:
            raise ValueError('unsupported abstract descriptor: ' + kind)
        self.edge(owner, 'Seed', into, atom)

    def obligation(self, owner, why):
        node = self.node()
        self.edge(owner, 'Seed', node, ('Opaque',))
        self.edge(owner, 'Export', node)
        self.unsupported.append(why)

    def instruction(self, name, pc, ins):
        owner = self.function_ids[name]
        op = ins[0]
        slot = lambda value: self.slot(name, value)
        site = f'{name}:{pc}:{op}'
        if op == 'fn':
            self.edge(owner, 'Seed', slot(ins[1]), ('Function', self.function_ids[ins[2]]))
        elif op == 'addr':
            self.edge(owner, 'Seed', slot(ins[1]), ('Address', self.cell_ids[ins[2]]))
        elif op in {'scalar', 'sub1'}:
            self.edge(owner, 'Seed', slot(ins[1]), ('Opaque',))
        elif op == 'input':
            self.descriptor(owner, slot(ins[1]), self.analyzer['inputs'][ins[2]])
        elif op == 'copy' or (op == 'view' and ins[3] in {'union_token', 'byte_token'}):
            self.edge(owner, 'Copy', slot(ins[2]), slot(ins[1]))
        elif op in {'expose', 'recover'}:
            # Atom::Address denotes provenance, never numeric address equality. Copy
            # preserves it across representation changes; Opaque stays Opaque. The
            # supplied API cannot validate conversion operand kinds or exposed bits,
            # so this unsupported validation has an explicit callable obligation.
            self.edge(owner, 'Copy', slot(ins[2]), slot(ins[1]))
            self.obligation(owner, site + ':representation-validity-not-modeled')
        elif op == 'store':
            self.edge(owner, 'Store', slot(ins[1]), slot(ins[2]))
        elif op == 'load':
            self.edge(owner, 'Load', slot(ins[2]), slot(ins[1]))
        elif op == 'call':
            self.edge(owner, 'Invoke', slot(ins[2]), tuple(slot(a) for a in ins[3]), slot(ins[1]))
        elif op == 'return':
            if ins[1] is not None:
                self.edge(owner, 'Copy', slot(ins[1]), self.returns[name])
            else:
                self.edge(owner, 'Seed', self.returns[name], ('Opaque',))
        elif op == 'branch':
            # Deliberately scan the entire body: no concrete predicate or jump index
            # selects a path, and neither a branch nor a return truncates extraction.
            pass
        elif op == 'effect':
            require(ins[1] in EFFECT_BITS, 'unmapped effect: ' + ins[1])
        else:
            if len(ins) > 1 and isinstance(ins[1], str):
                self.edge(owner, 'Seed', slot(ins[1]), ('Opaque',))
            self.obligation(owner, site + ':unsupported-operation')

    def translate(self):
        for name in self.cells:
            self.descriptor(self.root, self.cell_nodes[name], self.program['cells'][name]['initial'])
        entry_params = self.program['functions'][self.program['entry']]['params']
        require(len(self.analyzer['args']) == len(entry_params), 'entry argument arity mismatch')
        for param, descriptor in zip(entry_params, self.analyzer['args']):
            self.descriptor(self.root, self.slots[self.program['entry'], param], descriptor)
        for name in self.functions:
            for pc, ins in enumerate(self.program['functions'][name]['body']):
                self.instruction(name, pc, ins)
        return {'root': self.root, 'bodies': self.bodies, 'cells': len(self.cells),
                'nodes': self.count, 'edges': self.edges, 'unsupported': self.unsupported}


def expectation(case_id, actual, function_names):
    # Actual outputs, not authored expectations, supply the coverage witness. The
    # result's status remains separate from callable closure and termination.
    targets = sorted({function_names.index(name) for name in actual['invoked_targets']})
    effects = {effect['label'] for effect in actual['effects']}
    mask = sum(1 << EFFECT_BITS[label] for label in effects)
    if case_id in POSITIVE:
        require(actual['status'] == 'complete' and actual['failure'] is None, 'positive not complete')
        closure = 'Closed'
    elif case_id in EXPOSURE_OPEN or case_id in FAILED_OPEN:
        closure = 'Open'
    elif case_id == 'fuel_exhausted':
        require(actual['status'] == 'incomplete', 'fuel record status changed')
        closure = 'Either'  # Callable closure says nothing about termination.
    else:
        raise ValueError('case lacks an explicit closure policy: ' + case_id)
    return {'targets': targets, 'effects': mask, 'closure': closure,
            'status': actual['status'], 'failure': actual['failure']}


def accepted(expect, closed, active, effects):
    """Only the fixture assertion predicate, never a solver."""
    if expect['closure'] == 'Closed' and not closed:
        return False
    if expect['closure'] == 'Open' and closed:
        return False
    return not closed or (all(t < len(active) and active[t] for t in expect['targets'])
                          and effects & expect['effects'] == expect['effects'])


def rust_atom(atom):
    return 'FixtureAtom::' + atom[0] + (f'({atom[1]})' if len(atom) > 1 else '')


def rust_edge(edge):
    owner, kind, *args = edge
    if kind == 'Seed':
        values = f'{args[0]}, {rust_atom(args[1])}'
    elif kind == 'Invoke':
        values = f'{args[0]}, &{list(args[1])}, {args[2]}'
    else:
        values = ', '.join(str(a) for a in args)
    return f'        FixtureEdge({owner}, FixtureOp::{kind}({values})),\n'


def render_fixture(cases, seals, frozen):
    header = [
        '// @generated by corpus/generate.py; do not edit.',
        '// From callable/:',
        '// python3 -B corpus/generate.py \'{"schema":"callable-fixture-source/1","mode":"generate","output":"fixtures_data.rs"}\'',
        '// Check: use the same command with mode=check (no writes).',
        '// SHA256 corpus/inputs.json: ' + BUNDLE_SHA256,
        '// Frozen source commit: ' + frozen['commit'],
        '// Frozen source tree: ' + frozen['tree'],
        '// Oracle was NOT rerun. Expectations use recorded actual outputs.',
        '// Graph extraction uses only analyzer-inputs; never corpus.execution or actual witnesses.',
        '// Scope: whole-token callable graph, not MIR/Rust pointer validity or termination.',
        '// Effects: io.write = 1u32 << 0; unwind = 1u32 << 1 (2 of at most 32 labels).',
    ]
    for path in ['SHA256SUMS', 'corpus.json', 'self-test-result.json', 'interpreter.py']:
        header.append('// SHA256 ' + path + ': ' + seals[path])
    result = '\n'.join(header) + '\n\n#[rustfmt::skip]\nstatic FIXTURES: &[Fixture] = &[\n'
    for case in cases:
        graph, expect = case['graph'], case['expect']
        failure = expect['failure']['code'] if expect['failure'] else 'none'
        result += f'    // {case["id"]}: concrete status={expect["status"]}; failure={failure}.\n'
        result += '// analyzer-input sha256: ' + seals['analyzer-inputs/' + case['id'] + '.json'] + '\n'
        for why in graph['unsupported']:
            result += '    // Incomplete obligation: ' + why + '\n'
        result += f'    Fixture {{ name: "{case["id"]}", root: {graph["root"]}, nodes: {graph["nodes"]}, cells: {graph["cells"]},\n'
        result += '        functions: &[\n'
        for body in graph['bodies']:
            result += f'            FixtureFunction {{ name: "{body["name"]}", arity: {body["arity"]}, effects: {body["effects"]} }},\n'
        result += '        ], edges: &[\n'
        result += ''.join(rust_edge(edge) for edge in graph['edges'])
        result += f'        ], targets: &{expect["targets"]}, effects: {expect["effects"]}, closure: Closure::{expect["closure"]},\n'
        result += f'        status: "{expect["status"]}", failure: "{failure}" }},\n'
    return (result + '];\n').encode()


def structure_checks(cases):
    require(len(EFFECT_BITS) <= 32 and len(set(EFFECT_BITS.values())) == len(EFFECT_BITS),
            'effect bits must be unique and fit u32')
    require(all(0 <= bit < 32 for bit in EFFECT_BITS.values()), 'effect bit outside u32')
    require(len(cases) == 20, 'expected all 20 cases')
    require(sum(c['expect']['closure'] == 'Closed' for c in cases) == 11, 'positive count mismatch')
    for case in cases:
        graph = case['graph']
        require(graph['nodes'] > 0, 'empty fixture graph')
        require(all(0 <= e[0] < len(graph['bodies']) for e in graph['edges']), 'invalid edge owner')
        # There is no abstract execution here: validate only static indices/syntax.
        for edge in graph['edges']:
            kind, args = edge[1], edge[2:]
            operands = (args[0], *args[1], args[2]) if kind == 'Invoke' else (args[0],) if kind == 'Seed' else args
            require(all(0 <= n < graph['nodes'] for n in operands), 'invalid node index')
        if graph['unsupported']:
            require(sum(e[1] == 'Export' for e in graph['edges']) == len(graph['unsupported']), 'missing opaque obligation')
    by_id = {c['id']: c for c in cases}
    require(by_id['mixed_branch_effectful']['graph'] == by_id['mixed_branch_pure']['graph'], 'concrete branch leaked')
    require(any(e[1] == 'Seed' and e[-1] == ('Opaque',) for e in by_id['external_raw_memory_callback']['graph']['edges']), 'raw input not opaque')
    require(not any(e[1] == 'Seed' and e[-1][0] == 'Function' for e in by_id['external_raw_memory_callback']['graph']['edges']), 'hidden raw callback leaked')
    control = by_id['byte_full_token_alias']
    expect = control['expect']
    active = [True] * len(control['graph']['bodies'])
    require(accepted(expect, True, active, expect['effects']), 'positive control rejected')
    nonroot = next(t for t in expect['targets'] if t != control['graph']['root'])
    active[nonroot] = False
    require(not accepted(expect, True, active, expect['effects']), 'missing target escaped')
    active[nonroot] = True
    require(not accepted(expect, True, active, 0), 'missing effect escaped')
    require(not all(accepted(c['expect'], False, [], 0) for c in cases), 'all-incomplete escaped')
    # Dependency boundary: the adapter takes precisely analyzer input, and the only
    # imports in this generator are stdlib. No oracle module is imported/executed.
    tree = ast.parse(Path(__file__).read_text())
    forbidden = {'subprocess', 'interpreter', 'self_test', 'ctypes', 'socket'}
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            require(not any(a.name.split('.')[0] in forbidden for a in node.names), 'forbidden import')
        if isinstance(node, ast.ImportFrom):
            require(node.module not in forbidden, 'forbidden import')
    return {'missing_target_rejected': True, 'missing_effect_rejected': True,
            'all_incomplete_rejected': True, 'branches_join_without_witness': True,
            'raw_callback_not_seeded': True, 'scope': 'generator structure/assertion predicate only; no solver executed'}


def fixture_cases(bundle):
    files = bundle['files']
    corpus = files['corpus.json']
    actual_result = files['self-test-result.json']
    require(actual_result['passed'] and not actual_result['failures'], 'recorded oracle self-test failed')
    require(actual_result['corpus_sha256'] == CORPUS_SHA256, 'recorded corpus identity mismatch')
    require(actual_result['interpreter_sha256'] == ORACLE_SHA256, 'recorded oracle identity mismatch')
    authored_rows = corpus['cases'] + corpus['error_cases']
    authored = {c['id']: c for c in authored_rows}
    records = {r['case']: r for r in actual_result['records']}
    require(len(authored_rows) == len(authored) == len(actual_result['records']) == len(records) == 20,
            'expected exactly 20 unique authored/actual cases')
    require(set(authored) == set(records) == POSITIVE | EXPOSURE_OPEN | FAILED_OPEN | {'fuel_exhausted'},
            'case identities or explicit closure policies differ')
    analyzer_paths = {'analyzer-inputs/' + case_id + '.json' for case_id in records}
    require(set(files) == analyzer_paths | {'corpus.json', 'self-test-result.json', 'checkout-before.json'},
            'bundled input files differ from required source set')
    cases = []
    for path in sorted(analyzer_paths):
        analyzer = files[path]
        case_id = analyzer['case_id']
        require(path == 'analyzer-inputs/' + case_id + '.json', 'analyzer identity mismatch')
        record = records[case_id]
        require(record['matched_manual_expected'], 'record failed authored expectations: ' + case_id)
        for key, value in authored[case_id]['expected'].items():
            require(record['actual'][key] == value, 'actual differs from authored ' + case_id + ':' + key)
        graph = Adapter(analyzer).translate()  # Only independent analyzer data crosses this boundary.
        expect = expectation(case_id, record['actual'], [b['name'] for b in graph['bodies']])
        cases.append({'id': case_id, 'graph': graph, 'expect': expect})
    return cases


def generate(request):
    source = Path(__file__).resolve().parent
    output = Path(request.output).resolve()
    require(output != source and source not in output.parents, 'output must be outside corpus/ source')
    bundle, seals = read_sealed()
    cases = fixture_cases(bundle)
    controls = structure_checks(cases)
    identity = bundle['files']['checkout-before.json']
    fixture = render_fixture(cases, seals, identity['frozen'])
    second_bundle, second_seals = read_sealed()
    require(fixture == render_fixture(fixture_cases(second_bundle), second_seals, identity['frozen']),
            'independent translation/render is not reproducible')
    if request.mode == 'check':
        require(output.read_bytes() == fixture, 'reproducibility mismatch: ' + str(output))
    else:
        require(output.parent.is_dir(), 'output parent directory must already exist: ' + str(output.parent))
        output.write_bytes(fixture)
    require(read_sealed() == (bundle, seals), 'sealed inputs changed during generation')
    return {'passed': True, 'mode': request.mode, 'cases': len(cases),
            'positive_closed_required': 11, 'open_required': 8, 'prefix_only_closure_either': 1,
            'fixture_sha256': digest(fixture), 'inputs_sha256': BUNDLE_SHA256,
            'authored_expectations_verified': 20, 'controls': controls,
            'reproducibility': 'two independent translations/renders byte-identical',
            'oracle_rerun': False, 'solver_executed': False}


def main():
    require(len(sys.argv) == 2, 'pass exactly one JSON request, path, or -')
    argument = sys.argv[1]
    raw = sys.stdin.read() if argument == '-' else argument if argument.lstrip().startswith('{') else Path(argument).read_text()
    result = generate(Request.parse(load_json(raw)))
    sys.stdout.write(encoded(result).decode())


if __name__ == '__main__':
    main()
