// TCP forwarder: 127.0.0.1:5672 -> <host>:<port>.
// The Java client's test suite connects to localhost:5672 (ConnectionFactory defaults), so inside the
// test container we listen there and relay to the broker. Run with `java Forward.java <host> <port>`.
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;

public class Forward {
  public static void main(String[] args) throws Exception {
    String host = args[0];
    int port = Integer.parseInt(args[1]);
    ServerSocket server = new ServerSocket();
    server.setReuseAddress(true);
    server.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 5672), 512);
    System.out.println("forwarding 127.0.0.1:5672 -> " + host + ":" + port);
    while (true) {
      Socket client = server.accept();
      Thread.ofVirtual().start(() -> {
        Socket backend = new Socket();
        try {
          backend.connect(new InetSocketAddress(host, port), 5000);
        } catch (Exception e) {
          close(client);
          return;
        }
        for (Socket s : new Socket[] {client, backend}) {
          try {
            s.setTcpNoDelay(true);
          } catch (Exception ignored) {
          }
        }
        Thread a = Thread.ofVirtual().start(() -> pipe(client, backend));
        pipe(backend, client);
        try {
          a.join();
        } catch (InterruptedException ignored) {
        }
        close(client);
        close(backend);
      });
    }
  }

  // Copy until EOF, then half-close the destination; on error, tear down both sides.
  static void pipe(Socket from, Socket to) {
    byte[] buf = new byte[65536];
    try {
      InputStream in = from.getInputStream();
      OutputStream out = to.getOutputStream();
      int n;
      while ((n = in.read(buf)) >= 0) {
        out.write(buf, 0, n);
      }
      to.shutdownOutput();
    } catch (Exception e) {
      close(from);
      close(to);
    }
  }

  static void close(Socket s) {
    try {
      s.close();
    } catch (Exception ignored) {
    }
  }
}
