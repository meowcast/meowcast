// Bounded Rockusb DRAM operations. No flash write/erase or efuse opcodes.
// Wire format: linux-rockchip/rkflashtool; RKComm.h in official rkdeveloptool.
#include <libusb.h>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

static void le32(unsigned char *p,uint32_t v) { for(int i=0;i<4;++i)p[i]=(v>>(8*i))&255; }
static void be32(unsigned char *p,uint32_t v) { for(int i=0;i<4;++i)p[3-i]=(v>>(8*i))&255; }
static uint32_t rd32(const unsigned char *p) { return uint32_t(p[0])|uint32_t(p[1])<<8|uint32_t(p[2])<<16|uint32_t(p[3])<<24; }
static bool in_range(uint32_t a,uint32_t n,uint32_t begin,uint32_t end) {
    return a>=begin && uint64_t(a)+n<=end;
}
static bool readable(uint32_t a,uint32_t n) {
    // Direct USB reads from MMIO are deliberately excluded: the helper stalled
    // during a CRU register read. Future CPU-based peripheral access is separate.
    return in_range(a,n,0x60000000,0x70000000);
}

class Device {
    libusb_context *context=nullptr;
    libusb_device_handle *handle=nullptr;
    int interface_number=-1;
    unsigned char input=0,output=0;
    uint32_t tag=0x52414d30;
public:
    ~Device() {
        if(handle) { if(interface_number>=0)libusb_release_interface(handle,interface_number); libusb_close(handle); }
        if(context)libusb_exit(context);
    }
    bool open() {
        int e=libusb_init(&context);
        if(e) return false;
        libusb_device **devices=nullptr;
        ssize_t count=libusb_get_device_list(context,&devices);
        libusb_device *selected=nullptr; int matches=0;
        for(ssize_t i=0;i<count;++i) {
            libusb_device_descriptor d{};
            if(!libusb_get_device_descriptor(devices[i],&d) && d.idVendor==0x2207 && d.idProduct==0x320b) { selected=devices[i]; ++matches; }
        }
        if(matches!=1) { std::fprintf(stderr,"Expected one RK322x; found %d\n",matches); libusb_free_device_list(devices,1); return false; }
        e=libusb_open(selected,&handle);
        libusb_config_descriptor *config=nullptr;
        if(!e)e=libusb_get_active_config_descriptor(selected,&config);
        if(e) { std::fprintf(stderr,"open/config: %s\n",libusb_error_name(e)); libusb_free_device_list(devices,1); return false; }
        for(int i=0;i<config->bNumInterfaces;++i) for(int j=0;j<config->interface[i].num_altsetting;++j) {
            const auto &d=config->interface[i].altsetting[j];
            if(d.bInterfaceClass!=0xff || d.bInterfaceSubClass!=6 || d.bInterfaceProtocol!=5)continue;
            unsigned char ei=0,eo=0;
            for(int k=0;k<d.bNumEndpoints;++k) {
                const auto &ep=d.endpoint[k];
                if((ep.bmAttributes&3)!=LIBUSB_TRANSFER_TYPE_BULK)continue;
                if(ep.bEndpointAddress&0x80)ei=ep.bEndpointAddress; else eo=ep.bEndpointAddress;
            }
            if(ei&&eo) { interface_number=d.bInterfaceNumber; input=ei; output=eo; }
        }
        libusb_free_config_descriptor(config); libusb_free_device_list(devices,1);
        if(interface_number<0)return false;
        e=libusb_claim_interface(handle,interface_number);
        if(e) { std::fprintf(stderr,"claim: %s\n",libusb_error_name(e)); interface_number=-1; return false; }
        return true;
    }
    bool transfer(unsigned char opcode,uint32_t address,std::vector<unsigned char> &data) {
        bool read=opcode==0x17;
        if(opcode!=0x17 && opcode!=0x18 && opcode!=0x19)return false;
        if(opcode==0x18) {
            // The stock helper zeros 512 bytes after every RAM write. Refuse
            // precise writes unless the known padding-call NOP patch is present.
            std::vector<unsigned char> guard(8);
            const unsigned char nops[4]={0x00,0xbf,0x00,0xbf};
            if(!transfer(0x17,0x63002a58,guard) || std::memcmp(guard.data()+2,nops,4)) {
                std::fprintf(stderr,"Refusing write: verified no-padding helper required\n"); return false;
            }
        }
        unsigned char request[31]{};
        std::memcpy(request,"USBC",4); le32(request+4,++tag); le32(request+8,data.size());
        request[12]=read?0x80:0; request[14]=10; request[15]=opcode;
        // This RK322x helper adds 0x60000000 to READ/WRITE_SDRAM addresses.
        // Present physical addresses at the CLI, but transmit DRAM offsets.
        be32(request+17,address-0x60000000);
        if(opcode==0x19)request[16]=0xaa; // Invoke registered vendor callback.
        if(opcode!=0x19) { request[22]=data.size()>>8; request[23]=data.size()&255; }
        int n=0;
        int e=libusb_bulk_transfer(handle,output,request,sizeof(request),&n,5000);
        if(e || n!=31) { std::fprintf(stderr,"request: %s (%d bytes)\n",libusb_error_name(e),n); return false; }
        if(!data.empty()) {
            n=0;
            e=libusb_bulk_transfer(handle,read?input:output,data.data(),data.size(),&n,5000);
            if(e || n!=static_cast<int>(data.size())) {
                std::fprintf(stderr,"data: %s (%d/%zu bytes); prefix=",libusb_error_name(e),n,data.size());
                for(int i=0;i<n&&i<16;++i)std::fprintf(stderr,"%02x",data[i]);
                std::fprintf(stderr,"\n"); return false;
            }
        }
        unsigned char status[13]{}; n=0;
        e=libusb_bulk_transfer(handle,input,status,sizeof(status),&n,opcode==0x19?60000:10000);
        if(e || n!=13 || std::memcmp(status,"USBS",4) || rd32(status+4)!=tag || rd32(status+8) || status[12]) {
            std::fprintf(stderr,"status: %s bytes=%d raw=",libusb_error_name(e),n);
            for(int i=0;i<n&&i<13;++i)std::fprintf(stderr,"%02x",status[i]);
            std::fprintf(stderr,"\n"); return false;
        }
        return true;
    }
};

int main(int argc,char **argv) {
    if(argc<3)return 2;
    char *end=nullptr;
    uint32_t address=std::strtoul(argv[2],&end,0);
    if(!end || *end || (address&3))return 2;
    std::vector<unsigned char> data;
    unsigned char opcode=0;
    if(!std::strcmp(argv[1],"read") && argc==5) {
        uint32_t length=std::strtoul(argv[3],&end,0);
        if(*end || !length || length>16384 || !readable(address,length))return 2;
        data.resize(length); opcode=0x17;
    } else if(!std::strcmp(argv[1],"write-ram") && argc==4) {
        FILE *f=std::fopen(argv[3],"rb"); if(!f)return 2;
        std::fseek(f,0,SEEK_END); long length=std::ftell(f); std::rewind(f);
        if(length<=0 || length>16384 || !in_range(address,length,0x62000000,0x64000000)) { std::fclose(f); return 2; }
        data.resize(length);
        bool ok=std::fread(data.data(),1,length,f)==static_cast<size_t>(length); std::fclose(f); if(!ok)return 2;
        opcode=0x18;
    } else if(!std::strcmp(argv[1],"execute-scratch") && argc==3 && address==0x62001000) {
        opcode=0x19;
    } else return 2;
    Device device; if(!device.open())return 1;
    if(!device.transfer(opcode,address,data))return 1;
    if(opcode==0x17) {
        FILE *f=std::fopen(argv[4],"wbx"); if(!f)return 1;
        bool ok=std::fwrite(data.data(),1,data.size(),f)==data.size(); std::fclose(f); if(!ok)return 1;
    }
    std::printf("opcode=%02x address=%08x length=%zu success\n",opcode,address,data.size());
    return 0;
}
