# SIMIT fixtures

Tests read these instead of calling SIMIT. `question.json` has the real shape of the captcha
service answer. **The lookup fixtures are synthetic**: they follow the field names the
parser expects (`multas`, `numeroComparendo`, `infracciones[].codigoInfraccion`, `valorPagar`,
`fechaComparendo`, `estadoComparendo`, ...), but they were not captured from the live site.

Before release, replace them with real responses recorded from your own plates (mask nothing in
the JSON structure, but swap personal names/IDs for fake values) and keep these file names, so the
tests keep passing against real data. A failing parser test after a re-record means SIMIT changed.

| File | Scenario |
| --- | --- |
| `one_pending.json` | One pending C02 fine for TTO08G |
| `one_pending_updated_plus_new.json` | Same fine with new amount, plus a new D12 fine |
| `no_fines.json` | Plate with nothing pending |
| `question.json` | Proof-of-work question, real shape (from browser traffic) |
| `captcha.html` | Human verification page instead of JSON |
| `unexpected_shape.json` | JSON the parser does not recognise (site changed) |
