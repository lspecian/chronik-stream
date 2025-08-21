import java.net.Socket;
import java.io.OutputStream;
import java.io.InputStream;
import java.nio.ByteBuffer;

public class TestInitProducerId {
    public static void main(String[] args) throws Exception {
        Socket socket = new Socket("localhost", 9093);
        OutputStream out = socket.getOutputStream();
        InputStream in = socket.getInputStream();
        
        // Send InitProducerId request (API key 22)
        ByteBuffer req = ByteBuffer.allocate(50);
        req.putInt(25); // size (adjust based on actual content)
        req.putShort((short) 22); // InitProducerId
        req.putShort((short) 0);  // version 0
        req.putInt(1); // correlation id
        req.putShort((short) 11); // client_id length
        req.put("test-client".getBytes());
        req.putShort((short) -1); // transactional_id (null)
        req.putInt(60000); // transaction_timeout_ms
        
        out.write(req.array(), 0, 29); // Size of the actual request
        
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
        
        short errorCode = respBuf.getShort();
        System.out.println("Error code: " + errorCode);
        
        if (errorCode == 0) {
            long producerId = respBuf.getLong();
            short producerEpoch = respBuf.getShort();
            System.out.println("Producer ID: " + producerId);
            System.out.println("Producer epoch: " + producerEpoch);
            System.out.println("SUCCESS: InitProducerId working!");
        } else {
            System.out.println("FAILED: Error code " + errorCode);
        }
        
        socket.close();
    }
}