import java.net.Socket;
import java.io.OutputStream;
import java.io.InputStream;
import java.nio.ByteBuffer;

public class TestInitProducerIdV4 {
    public static void main(String[] args) throws Exception {
        Socket socket = new Socket("localhost", 9092);
        OutputStream out = socket.getOutputStream();
        InputStream in = socket.getInputStream();
        
        // Send InitProducerId request v4 (what kafka-console-producer uses)
        ByteBuffer req = ByteBuffer.allocate(50);
        req.putInt(25); // size
        req.putShort((short) 22); // InitProducerId
        req.putShort((short) 4);  // version 4 (what the error report shows)
        req.putInt(1); // correlation id
        req.putShort((short) 11); // client_id length
        req.put("test-client".getBytes());
        req.putShort((short) -1); // transactional_id (null)
        req.putInt(60000); // transaction_timeout_ms
        
        out.write(req.array(), 0, 29);
        
        // Read response size
        byte[] sizeBytes = new byte[4];
        in.read(sizeBytes);
        int respSize = ByteBuffer.wrap(sizeBytes).getInt();
        System.out.println("Response size: " + respSize);
        
        // Read response
        byte[] response = new byte[respSize];
        in.read(response);
        
        // Parse response
        ByteBuffer respBuf = ByteBuffer.wrap(response);
        int correlationId = respBuf.getInt();
        System.out.println("Correlation ID: " + correlationId);
        
        // For v4, throttle time comes first
        int throttleTimeMs = respBuf.getInt();
        System.out.println("Throttle time ms: " + throttleTimeMs);
        
        short errorCode = respBuf.getShort();
        System.out.println("Error code: " + errorCode);
        
        if (errorCode == 0) {
            long producerId = respBuf.getLong();
            short producerEpoch = respBuf.getShort();
            System.out.println("Producer ID: " + producerId);
            System.out.println("Producer epoch: " + producerEpoch);
            System.out.println("✅ SUCCESS: InitProducerId v4 working!");
        } else {
            System.out.println("❌ FAILED: Error code " + errorCode);
        }
        
        socket.close();
    }
}