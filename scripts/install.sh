#!/bin/bash
# Chronik Stream - Simple Installation Script

set -e

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${GREEN}Chronik Stream - Installation${NC}"
echo "=============================="
echo ""

# Detect OS
if [[ "$OSTYPE" == "linux-gnu"* ]]; then
    OS="linux"
elif [[ "$OSTYPE" == "darwin"* ]]; then
    OS="macos"
else
    echo -e "${RED}Unsupported OS: $OSTYPE${NC}"
    exit 1
fi

# Installation method selection
echo "Choose installation method:"
echo "1) Docker (recommended)"
echo "2) Snap package (Linux only)"
echo "3) Binary download"
echo "4) Build from source"
echo ""
read -p "Enter choice [1-4]: " choice

case $choice in
    1) # Docker installation
        echo -e "${YELLOW}Installing with Docker...${NC}"
        
        # Check if Docker is installed
        if ! command -v docker &> /dev/null; then
            echo -e "${RED}Docker is not installed!${NC}"
            echo "Please install Docker first: https://docs.docker.com/get-docker/"
            exit 1
        fi
        
        # Pull the all-in-one image
        echo "Pulling Chronik Stream image..."
        docker pull chronikstream/chronik:latest
        
        # Create data directory
        mkdir -p ~/.chronik/data
        
        # Create convenience script
        cat > /tmp/chronik << 'EOF'
#!/bin/bash
docker run -d \
    --name chronik \
    -p 9092:9092 \
    -p 3000:3000 \
    -p 9090:9090 \
    -v ~/.chronik/data:/data \
    --restart unless-stopped \
    chronikstream/chronik:latest "$@"
EOF
        
        if [[ "$OS" == "linux" ]]; then
            sudo mv /tmp/chronik /usr/local/bin/chronik
            sudo chmod +x /usr/local/bin/chronik
        else
            mv /tmp/chronik /usr/local/bin/chronik
            chmod +x /usr/local/bin/chronik
        fi
        
        echo -e "${GREEN}✓ Installed successfully!${NC}"
        echo ""
        echo "Start Chronik Stream with: chronik start"
        echo "Stop with: docker stop chronik"
        ;;
        
    2) # Snap installation
        if [[ "$OS" != "linux" ]]; then
            echo -e "${RED}Snap is only available on Linux${NC}"
            exit 1
        fi
        
        echo -e "${YELLOW}Installing with Snap...${NC}"
        
        # Check if snap is installed
        if ! command -v snap &> /dev/null; then
            echo "Installing snapd..."
            sudo apt update && sudo apt install -y snapd
        fi
        
        # Install the snap
        sudo snap install chronik-stream
        
        echo -e "${GREEN}✓ Installed successfully!${NC}"
        echo ""
        echo "Chronik Stream is running as a service"
        echo "Check status with: snap services chronik-stream"
        ;;
        
    3) # Binary download
        echo -e "${YELLOW}Downloading binary...${NC}"
        
        # Determine architecture
        ARCH=$(uname -m)
        case $ARCH in
            x86_64) ARCH="x86_64" ;;
            aarch64|arm64) ARCH="aarch64" ;;
            *) echo -e "${RED}Unsupported architecture: $ARCH${NC}"; exit 1 ;;
        esac
        
        # Download latest release
        LATEST_URL="https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-${OS}-${ARCH}"
        
        echo "Downloading from: $LATEST_URL"
        curl -L "$LATEST_URL" -o /tmp/chronik
        
        # Install binary
        if [[ "$OS" == "linux" ]]; then
            sudo mv /tmp/chronik /usr/local/bin/chronik
            sudo chmod +x /usr/local/bin/chronik
        else
            mv /tmp/chronik /usr/local/bin/chronik
            chmod +x /usr/local/bin/chronik
        fi
        
        echo -e "${GREEN}✓ Installed successfully!${NC}"
        echo ""
        echo "Start Chronik Stream with: chronik start"
        ;;
        
    4) # Build from source
        echo -e "${YELLOW}Building from source...${NC}"
        
        # Check if Rust is installed
        if ! command -v cargo &> /dev/null; then
            echo "Installing Rust..."
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
            source $HOME/.cargo/env
        fi
        
        # Clone repository
        if [ ! -d "chronik-stream" ]; then
            git clone https://github.com/lspecian/chronik-stream.git
        fi
        
        cd chronik-stream
        
        # Build the all-in-one binary
        echo "Building Chronik Stream..."
        cargo build --release --bin chronik --package chronik-all-in-one
        
        # Install
        if [[ "$OS" == "linux" ]]; then
            sudo cp target/release/chronik /usr/local/bin/
        else
            cp target/release/chronik /usr/local/bin/
        fi
        
        echo -e "${GREEN}✓ Built and installed successfully!${NC}"
        echo ""
        echo "Start Chronik Stream with: chronik start"
        ;;
        
    *)
        echo -e "${RED}Invalid choice${NC}"
        exit 1
        ;;
esac

echo ""
echo -e "${GREEN}Installation complete!${NC}"
echo ""
echo "Quick start:"
echo "  1. Start server:  chronik start"
echo "  2. Check status:  chronik check"
echo "  3. Test connection:"
echo "     kafkactl --brokers localhost:9092 get topics"
echo ""
echo "For more information, visit: https://github.com/lspecian/chronik-stream"