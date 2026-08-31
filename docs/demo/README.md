# Demo recording

`scripts/render_demo.py` records `agul.tape` against a deterministic local
OpenAI-compatible fixture. It is a product walkthrough, not a provider or
runtime benchmark.

Before VHS opens the visible terminal, the script runs the same prompt through
the real Agul binary once with output captured. That hidden turn performs the
same four model rounds and three real file tools. The fixture keeps those four
request bodies as its warmed prefixes. During recording, a request reports a
cache hit only when its complete JSON body exactly matches the corresponding
warm-up request; a mismatch returns HTTP 409 and fails the recording.

For a compact visual example, the deterministic fixture assigns each exact
replay a five-token uncached tail. This is declared demo data, not a measured
provider cache rate. The four visible rounds therefore report 7,180 cache-hit
tokens out of 7,200 input tokens, which the workbench renders as `KV 99.7%`.
The launcher declares the fixture's 32K window so the same exact response usage
also drives the visible `ctx` ratio. An unwarmed request reports zero cache-hit
tokens. `scripts/test_render_demo.py` fixes these values as executable fixture
semantics, and the renderer verifies the warm-up and visible request lists
again after VHS exits.

Regenerate the asset on Linux or macOS with VHS 0.11 or newer:

```console
python scripts/render_demo.py
```

## Candidate acceptance

When a local Linux or macOS recorder is unavailable, the `dev` workflow records
pushes that change the Workbench or Demo sources. After the workflow itself is
on the default branch, it can also be started manually from **Actions** for a
selected candidate ref. It builds and records exactly that ref with pinned
Rust, Python, and VHS versions, then exposes an `agul-demo` artifact for 14
days. The workflow has read-only repository permission: it never commits,
pushes, tags, or publishes anything.

Download and extract the `agul-demo` artifact from the completed run. After
visually checking the result, replace `docs/assets/agul-demo.gif` in the
candidate checkout with the downloaded `agul-demo.gif`. Review that file as a
normal candidate change before it is included in any later commit; downloading
the artifact alone does not alter the repository.
