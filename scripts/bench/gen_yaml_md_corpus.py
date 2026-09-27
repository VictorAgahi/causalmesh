#!/usr/bin/env python3
"""Deterministic YAML / Markdown corpus for the 4.9 per-file memory measurement.

Every file is generated just under the per-file budget the indexer applies to it
(`AstGuard::within_size_budget`, crates/mesh-parsers/src/guard.rs):

- `openapi.yaml` / `asyncapi.yaml` are schema-named, so they get the 1.5 MB
  budget (`MAX_SCHEMA_FILE_SIZE_BYTES` = 1_572_864 bytes);
- every other `.yml` / `.yaml` / `.md` gets 384 KB (`MAX_FILE_SIZE_BYTES` =
  393_216 bytes).

`openapi.json` is not generated: `.json` maps to `LanguageKind::Unknown`, so no
YAML/JSON parser ever reads it (only custom regex patterns, if configured).

Usage: gen_yaml_md_corpus.py <out_dir>
Writes one sub-directory per case (`<out_dir>/<case>/<file>`) so each case can be
indexed as a workspace holding that single file, plus `<out_dir>/empty/`.
No randomness, no network: the same invocation always writes the same bytes.
"""

import os
import sys

SCHEMA_BUDGET = 1536 * 1024
SOURCE_BUDGET = 384 * 1024
# Stay a little under the budget so the file is never rejected as oversized.
MARGIN = 2048


def fill(target, header, block_fn, footer=""):
    """Appends `block_fn(i)` until the next block would cross `target` bytes."""
    parts = [header]
    size = len(header.encode()) + len(footer.encode())
    i = 0
    while True:
        block = block_fn(i)
        n = len(block.encode())
        if size + n > target:
            break
        parts.append(block)
        size += n
        i += 1
    parts.append(footer)
    return "".join(parts), i


def openapi_yaml():
    header = (
        "openapi: 3.0.3\n"
        "info:\n  title: Generated Shop API\n  version: '1.0.0'\n"
        "x-defaults:\n"
        "  std-responses: &stdResponses\n"
        "    '400': {description: Bad request}\n"
        "    '401': {description: Unauthorized}\n"
        "    '500': {description: Internal error}\n"
        "  paging: &paging\n"
        "    - {name: limit, in: query, schema: {type: integer, maximum: 500}}\n"
        "    - {name: cursor, in: query, schema: {type: string}}\n"
        "paths:\n"
    )

    def path_block(i):
        return (
            f"  /v1/resource{i:05d}/{{id}}:\n"
            f"    get:\n"
            f"      operationId: getResource{i:05d}\n"
            f"      summary: Fetch resource {i} by id, with paging and filters\n"
            f"      parameters: *paging\n"
            f"      responses:\n"
            f"        '200':\n"
            f"          description: OK\n"
            f"          content:\n"
            f"            application/json:\n"
            f"              schema: {{$ref: '#/components/schemas/Resource{i:05d}'}}\n"
            f"        <<: *stdResponses\n"
            f"    post:\n"
            f"      operationId: updateResource{i:05d}\n"
            f"      requestBody:\n"
            f"        content:\n"
            f"          application/json:\n"
            f"            schema: {{$ref: '#/components/schemas/Resource{i:05d}'}}\n"
            f"      responses: *stdResponses\n"
        )

    def schema_block(i):
        return (
            f"    Resource{i:05d}:\n"
            f"      type: object\n"
            f"      required: [id, name]\n"
            f"      properties:\n"
            f"        id: {{type: string, format: uuid}}\n"
            f"        name: {{type: string, maxLength: 120}}\n"
            f"        price: {{type: number, minimum: 0}}\n"
            f"        tags: {{type: array, items: {{type: string}}}}\n"
        )

    # Half the budget for paths, half for components.schemas.
    paths, n = fill(SCHEMA_BUDGET // 2, header, path_block)
    body, _ = fill(
        SCHEMA_BUDGET - MARGIN - len(paths.encode()),
        "components:\n  schemas:\n",
        schema_block,
    )
    return paths + body


def asyncapi_yaml():
    header = (
        "asyncapi: 2.6.0\n"
        "info:\n  title: Generated Event Bus\n  version: '1.0.0'\n"
        "x-defaults:\n"
        "  headers: &headers\n"
        "    type: object\n"
        "    properties:\n"
        "      traceparent: {type: string}\n"
        "      tenant: {type: string}\n"
        "channels:\n"
    )

    def channel_block(i):
        return (
            f"  shop.orders.v1.event{i:05d}:\n"
            f"    description: Order lifecycle event {i}, emitted by the order service\n"
            f"    subscribe:\n"
            f"      operationId: onEvent{i:05d}\n"
            f"      message:\n"
            f"        name: Event{i:05d}\n"
            f"        headers: *headers\n"
            f"        payload:\n"
            f"          type: object\n"
            f"          properties:\n"
            f"            orderId: {{type: string, format: uuid}}\n"
            f"            amount: {{type: number}}\n"
            f"            status: {{type: string, enum: [created, paid, shipped]}}\n"
        )

    out, _ = fill(SCHEMA_BUDGET - MARGIN, header, channel_block)
    return out


def application_yml():
    """Spring-style property file: nested mappings, a merge key, lots of scalars."""
    header = (
        "defaults: &defaults\n"
        "  timeout-ms: 3000\n"
        "  retries: 3\n"
        "  pool: {min: 2, max: 16}\n"
        "services:\n"
    )

    def svc(i):
        return (
            f"  svc{i:05d}:\n"
            f"    <<: *defaults\n"
            f"    url: http://svc{i:05d}.internal:8080/api\n"
            f"    enabled: true\n"
            f"    owner: team-{i % 17}\n"
        )

    out, _ = fill(SOURCE_BUDGET - MARGIN, header, svc)
    return out


def md_many_sections():
    def sec(i):
        return f"## Section {i}\n\nShort paragraph {i} about the payment flow.\n\n"

    out, _ = fill(SOURCE_BUDGET - MARGIN, "# Handbook\n\n", sec)
    return out


def md_one_section():
    def para(i):
        return (
            f"Paragraph {i}: the order service publishes an event after the payment "
            f"is captured; consumers must be idempotent.\n\n"
        )

    out, _ = fill(SOURCE_BUDGET - MARGIN, "# One huge section\n\n", para)
    return out


def md_code_block():
    def line(i):
        return f"    let value_{i} = compute(input_{i}, &config); // step {i}\n"

    out, _ = fill(
        SOURCE_BUDGET - MARGIN,
        "# Code dump\n\n```rust\nfn generated() {\n",
        line,
        "}\n```\n",
    )
    return out


def laughs_nested():
    """Classic billion laughs: 9 levels x 9 aliases (~387M leaf expansions)."""
    lines = ['l0: &l0 ["lol"]']
    for level in range(1, 10):
        refs = ", ".join([f"*l{level - 1}"] * 9)
        lines.append(f"l{level}: &l{level} [{refs}]")
    # Mapping form too, so the property flattener (which skips sequences but
    # follows mapping values) is exercised, not only the sequence form.
    lines.append("m0: &m0 {a: lol}")
    for level in range(1, 10):
        refs = ", ".join(f"k{j}: *m{level - 1}" for j in range(9))
        lines.append(f"m{level}: &m{level} {{{refs}}}")
    return "\n".join(lines) + "\n"


def laughs_wide(budget, top_key):
    """Alias fan-out that stays under serde_yaml's repetition limit.

    serde_yaml 0.9 only errors once alias jumps exceed 100 x the number of
    events (de.rs `jump`). One big anchored mapping replayed by many one-line
    aliases makes few jumps (one per alias) but each jump replays the whole
    anchor, so the work and the visitor output grow as anchor_keys x aliases.
    """
    half = budget // 2
    anchor, _ = fill(half, "base: &big\n", lambda i: f"  k{i:05d}: v\n")
    refs, _ = fill(
        budget - MARGIN - len(anchor.encode()),
        f"{top_key}:\n",
        lambda i: f"  p{i:05d}: *big\n",
    )
    return anchor + refs


CASES = {
    # case dir: (file name, generator)
    "openapi-yaml": ("openapi.yaml", openapi_yaml),
    "asyncapi-yaml": ("asyncapi.yaml", asyncapi_yaml),
    "application-yml": ("application.yml", application_yml),
    "md-many-sections": ("handbook.md", md_many_sections),
    "md-one-section": ("essay.md", md_one_section),
    "md-code-block": ("code.md", md_code_block),
    "laughs-nested": ("application.yml", laughs_nested),
    "laughs-wide-props": ("application.yml", lambda: laughs_wide(SOURCE_BUDGET, "services")),
    "laughs-wide-openapi": ("openapi.yaml", lambda: laughs_wide(SCHEMA_BUDGET, "paths")),
    # Small fan-outs of the same shape, to show how the cost grows with size
    # without having to let the full-size file exhaust the machine.
    "laughs-wide-openapi-8k": ("openapi.yaml", lambda: laughs_wide(8 * 1024, "paths")),
    "laughs-wide-openapi-16k": ("openapi.yaml", lambda: laughs_wide(16 * 1024, "paths")),
    "laughs-wide-openapi-32k": ("openapi.yaml", lambda: laughs_wide(32 * 1024, "paths")),
}


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: gen_yaml_md_corpus.py <out_dir>")
    out_dir = sys.argv[1]
    os.makedirs(os.path.join(out_dir, "empty"), exist_ok=True)
    for case, (name, gen) in CASES.items():
        case_dir = os.path.join(out_dir, case)
        os.makedirs(case_dir, exist_ok=True)
        data = gen().encode()
        with open(os.path.join(case_dir, name), "wb") as f:
            f.write(data)
        print(f"{case}/{name}\t{len(data)}")


if __name__ == "__main__":
    main()
