# quinn-holepunch

holepuncheable quic sockets through a rendevouz server

**WIP**

## usage

on a public machine:
```
quic-holepunch rendevouz
```

on a machine behind a nat:
```
quic-holepunch listen -r RENDEVOUZ_IP:3033 -p foo
```

on another machine behind another nat:
```
quic-holepunch connect-rendevouz -r RENDEVOUZ_IP:3033 -p foo
```
