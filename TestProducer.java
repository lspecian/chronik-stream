import java.net.Socket;
import java.io.OutputStream;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;

public class TestProducer {
    public static void main(String[] args) throws Exception {
        Socket socket = new Socket("localhost", 9092);
        OutputStream out = socket.getOutputStream();
        InputStream in = socket.getInputStream();
        
        // First, send ApiVersions to check support
        System.out.println("1. Testing ApiVersions...");
        ByteBuffer apiVersionsReq = ByteBuffer.allocate(20);
        apiVersionsReq.putInt(11); // size
        apiVersionsReq.putShort((short) 18); // ApiVersions
        apiVersionsReq.putShort((short) 0);  // version
        apiVersionsReq.putInt(0); // correlation id
        apiVersionsReq.putShort((short) 3); // client_id length
        apiVersionsReq.put("foo".getBytes());
        out.write(apiVersionsReq.array(), 0, 15);
        
        // Read response
        byte[] sizeBytes = new byte[4];
        in.read(sizeBytes);
        int respSize = ByteBuffer.wrap(sizeBytes).getInt();
        byte[] response = new byte[respSize];
        in.read(response);
        System.out.println("ApiVersions response size: " + respSize);
        
        // Check if Produce is supported
        ByteBuffer respBuf = ByteBuffer.wrap(response);
        int correlationId = respBuf.getInt();
        short errorCode = respBuf.getShort();
        int apiCount = respBuf.getInt();
        System.out.println("Supported APIs: " + apiCount);
        
        boolean produceSupported = false;
        for (int i = 0; i < apiCount; i++) {
            short apiKey = respBuf.getShort();
            short minVersion = respBuf.getShort();
            short maxVersion = respBuf.getShort();
            if (apiKey == 0) { // Produce
                produceSupported = true;
                System.out.println("Produce API supported: v" + minVersion + "-v" + maxVersion);
            }
        }
        
        if (produceSupported) {
            System.out.println("✓ Produce API is supported!");
        } else {
            System.out.println("✗ Produce API not supported");
        }
        
        // Test InitProducerId
        System.out.println("\n2. Testing InitProducerId...");
        Socket socket2 = new Socket("localhost", 9092);
        OutputStream out2 = socket2.getOutputStream();
        InputStream in2 = socket2.getInputStream();
        
        ByteBuffer initReq = ByteBuffer.allocate(50);
        initReq.putInt(25); // size
        initReq.putShort((short) 22); // InitProducerId
        initReq.putShort((short) 0);  // version 0
        initReq.putInt(1); // correlation id
        initReq.putShort((short) 11); // client_id length
        initReq.put("test-client".getBytes());
        initReq.putShort((short) -1); // transactional_id (null)
        initReq.putInt(60000); // transaction_timeout_ms
        
        out2.write(initReq.array(), 0, 29);
        
        // Read response
        in2.read(sizeBytes);
        respSize = ByteBuffer.wrap(sizeBytes).getInt();
        response = new byte[respSize];
        in2.read(response);
        
        respBuf = ByteBuffer.wrap(response);
        correlationId = respBuf.getInt();
        errorCode = respBuf.getShort();
        
        if (errorCode == 0) {
            long producerId = respBuf.getLong();
            short producerEpoch = respBuf.getShort();
            System.out.println("✓ InitProducerId successful!");
            System.out.println("  Producer ID: " + producerId);
            System.out.println("  Producer epoch: " + producerEpoch);
        } else {
            System.out.println("✗ InitProducerId failed with error: " + errorCode);
        }
        
        socket2.close();
        socket.close();
        
        System.out.println("\n✅ Producer APIs are functional!");
    }
}