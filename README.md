# quinn-holepunch

holepuncheable quic sockets through a rendevouz server

**WIP**

## usage

on a public machine:
```
quinn-holepunch rendevouz
```

on a machine behind a nat:
```
quinn-holepunch listen -r RENDEVOUZ_IP:3033 -p foo
```

on another machine behind another nat:
```
quinn-holepunch connect-rendevouz -r RENDEVOUZ_IP:3033 -p foo
```
