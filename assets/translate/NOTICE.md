Vendored for page translation in the portal (served from `/translate/engine/`):

- `translator-worker.js`, `bergamot-translator-worker.js`, `bergamot-translator-worker.wasm` -
  `@browsermt/bergamot-translator` 0.4.9, MPL-2.0, https://github.com/browsermt/bergamot-translator
  (fetched from https://cdn.jsdelivr.net/npm/@browsermt/bergamot-translator@0.4.9/worker/, unmodified)

SHA-256:

    90257af9ee0758983501bdce77ca7ec168cd7360d4689298c20a81b0251d6b62  translator-worker.js
    748b2418418a2ffc6e70721aeb10098d8e6fb589ea156b91f4ae8bc3490d8f7a  bergamot-translator-worker.js
    95a2b58dd6773bf1b3f345d71f9149928b9f75f4ec9c9064c0b3e42c298671b2  bergamot-translator-worker.wasm

The language models are not part of this repository; `translation` packages fetch them from
Mozilla's Remote Settings (see `src/catalog/translation.rs`).
