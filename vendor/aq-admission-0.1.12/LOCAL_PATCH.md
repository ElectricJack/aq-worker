# Local interactive-session patch

This is the next source package for the explicit one-hour interactive policy. Previous vendor directories and archives remain unchanged. No archive/extracted-closure qualification is claimed here. The paired agent-q source supplies the admission policy implementation and focused test; terminal-protocol changes only its dependency/package version, preserving existing wire schemas. Policy2 and the default host permit remain capped at120000ms. Policy3 is selected only for the actual interactive factory and is bounded at3600000ms.


## 0.1.12 (MS1, aq-worker ms1/demo)

Mirror of aq-server `crates/aq-admission` 0.1.12: the long (schema 3) phase
policy also admits the docker-local Claude manifest (Claude 2.1.318,
`aq.claude-result-local-adapter`, terminal input off, container confinement
declared and native not declared). Everything else is byte-identical to 0.1.11.
