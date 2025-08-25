import java.net.Socket;
import java.nio.ByteBuffer;
import java.io.InputStream;
import java.io.OutputStream;

public class TestKafkaClient {
    public static void main(String[] args) throws Exception {
        Socket socket = new Socket("localhost", 9092);
        OutputStream out = socket.getOutputStream();
        InputStream in = socket.getInputStream();
        
        // Build Metadata request v0 (simplest version)
        ByteBuffer request = ByteBuffer.allocate(14);
        request.putShort((short) 3);  // API key (Metadata)
        request.putShort((short) 0);  // API version 0
        request.putInt(42);           // Correlation ID
        request.putShort((short) -1); // Client ID (null)
        request.putInt(-1);           // Topics (-1 = all)
        
        // Send request with size
        ByteBuffer sizeBuffer = ByteBuffer.allocate(4);
        sizeBuffer.putInt(request.position());
        out.write(sizeBuffer.array());
        out.write(request.array(), 0, request.position());
        out.flush();
        
        // Read response size
        byte[] sizeBytes = new byte[4];
        in.read(sizeBytes);
        int size = ByteBuffer.wrap(sizeBytes).getInt();
        System.out.println("Response size: " + size);
        
        // Read response
        byte[] response = new byte[size];
        int totalRead = 0;
        while (totalRead < size) {
            int read = in.read(response, totalRead, size - totalRead);
            if (read < 0) break;
            totalRead += read;
        }
        
        // Parse response
        ByteBuffer respBuffer = ByteBuffer.wrap(response);
        int correlationId = respBuffer.getInt();
        System.out.println("Correlation ID: " + correlationId);
        
        // For v0, no throttle_time_ms
        // Directly read brokers array
        int brokersLength = respBuffer.getInt();
        System.out.println("Brokers array length: " + brokersLength);
        
        if (brokersLength > 0) {
            int nodeId = respBuffer.getInt();
            System.out.println("  Node ID: " + nodeId);
            
            short hostLen = respBuffer.getShort();
            byte[] hostBytes = new byte[hostLen];
            respBuffer.get(hostBytes);
            System.out.println("  Host: " + new String(hostBytes));
            
            int port = respBuffer.getInt();
            System.out.println("  Port: " + port);
        }
        
        System.out.println("SUCCESS: Metadata response parsed correctly!");
        socket.close();
    }
}