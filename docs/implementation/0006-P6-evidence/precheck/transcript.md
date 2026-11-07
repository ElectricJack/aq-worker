### TOOL Bash {"command": "echo \"AQ_CHANNEL=$AQ_CHANNEL\"; echo \"PATH=$PATH\"; command -v aq; aq --version; id -u"}
### RESULT
AQ_CHANNEL=unix:/aq/channel/aq.sock
PATH=/opt/aq/bin:/usr/local/bin:/usr/bin:/bin
/opt/aq/bin/aq
aq 0.1.0
0
### TEXT
AQ_CHANNEL=unix:/aq/channel/aq.sock
PATH=/opt/aq/bin:/usr/local/bin:/usr/bin:/bin
/opt/aq/bin/aq
aq 0.1.0
0
### FINAL success is_error= False turns= 2 duration_ms= 3807
