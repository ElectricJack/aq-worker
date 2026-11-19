# Local interactive-session patch

This is the next source package for the explicit one-hour interactive policy. Previous vendor directories and archives remain unchanged. No archive/extracted-closure qualification is claimed here. The paired agent-q source supplies the admission policy implementation and focused test; terminal-protocol changes only its dependency/package version, preserving existing wire schemas. Policy2 and the default host permit remain capped at120000ms. Policy3 is selected only for the actual interactive factory and is bounded at3600000ms.

Admission dependency advanced to aq-admission 0.1.13 for bounded owner message input under the same container boundary. No phase-terminal wire bytes changed.
