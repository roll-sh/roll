import socket,errno,json,pathlib as P
R=P.Path(__file__).resolve().parent
port=53391
with socket.socket() as listener:
 listener.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);listener.bind(('127.0.0.1',port));listener.listen(1)
 # A reuse-aware guard must still refuse a live listener.
 try:
  with socket.socket() as probe:probe.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);probe.bind(('127.0.0.1',port))
 except OSError as e:assert e.errno==errno.EADDRINUSE;live_refused=True
 else:raise AssertionError('live listener accepted')
 with socket.create_connection(('127.0.0.1',port)) as client:
  accepted,_=listener.accept();accepted.close();assert client.recv(1)==b''
# This owned accepted socket closed first, placing its local port in TIME_WAIT.
try:
 with socket.socket() as probe:probe.bind(('127.0.0.1',port))
except OSError as e:assert e.errno==errno.EADDRINUSE;old_false_refusal=True
else:raise AssertionError('did not reproduce TIME_WAIT false refusal')
with socket.socket() as probe:probe.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);probe.bind(('127.0.0.1',port));new_guard_accepts=True
print(json.dumps(dict(owned_port=port,live_listener_refused=live_refused,old_guard_false_refusal=old_false_refusal,new_guard_accepts_timewait=new_guard_accepts)))
