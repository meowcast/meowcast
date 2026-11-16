// Diagnostic CDC ACM transport for the experimental Linux RAM image only.
// Supports description/configuration and an explicitly supplied shell command.
#include <libusb.h>
#include <cstdio>
#include <cstring>
#include <chrono>
#include <thread>

int main(int argc, char **argv) {
    if (argc < 2 || (std::strcmp(argv[1],"describe") && std::strcmp(argv[1],"console"))) return 2;
    bool console = !std::strcmp(argv[1],"console");
    if (console && argc != 3) return 2;
    libusb_context *ctx=nullptr;
    if (libusb_init(&ctx)) return 1;
    libusb_device **list=nullptr;
    ssize_t n=libusb_get_device_list(ctx,&list);
    libusb_device *dev=nullptr;
    int matches=0;
    for (ssize_t i=0;i<n;i++) {
        libusb_device_descriptor d{};
        if (!libusb_get_device_descriptor(list[i],&d) && d.idVendor==0x0525 && d.idProduct==0xa4a7) {
            dev=list[i]; ++matches;
        }
    }
    if(matches!=1) { std::fprintf(stderr,"Expected one Linux ACM gadget, got %d\n",matches); return 1; }
    libusb_device_handle *h=nullptr;
    int e=libusb_open(dev,&h);
    if(e) { std::fprintf(stderr,"open: %s\n",libusb_error_name(e)); return 1; }
    libusb_device_descriptor dd{};
    libusb_get_device_descriptor(dev,&dd);
    unsigned char manufacturer[256]{};
    e=libusb_get_string_descriptor_ascii(h,dd.iManufacturer,manufacturer,sizeof(manufacturer)-1);
    std::fprintf(stderr,"manufacturer: %s (%d)\n",manufacturer,e);
    if(e<0 || !std::strstr((char*)manufacturer,"Linux 7.3.4 with 30040000.usb")) return 1;
    int active=-1;
    e=libusb_get_configuration(h,&active);
    std::fprintf(stderr,"active configuration: %d (%s)\n",active,libusb_error_name(e));
    libusb_config_descriptor *cfg=nullptr;
    e=libusb_get_config_descriptor(dev,0,&cfg);
    if(e) { std::fprintf(stderr,"descriptor: %s\n",libusb_error_name(e)); return 1; }
    int control=-1,data=-1; unsigned char in=0,out=0;
    std::fprintf(stderr,"configuration value=%d total_length=%d interfaces=%d\n",cfg->bConfigurationValue,cfg->wTotalLength,cfg->bNumInterfaces);
    for(int i=0;i<cfg->bNumInterfaces;i++) {
        const auto &it=cfg->interface[i].altsetting[0];
        std::fprintf(stderr,"interface %d class=%02x subclass=%02x protocol=%02x endpoints=%d\n",
                     it.bInterfaceNumber,it.bInterfaceClass,it.bInterfaceSubClass,it.bInterfaceProtocol,it.bNumEndpoints);
        if(it.bInterfaceClass==2) control=it.bInterfaceNumber;
        if(it.bInterfaceClass==10) {
            data=it.bInterfaceNumber;
            for(int j=0;j<it.bNumEndpoints;j++) {
                const auto &ep=it.endpoint[j];
                std::fprintf(stderr," endpoint=%02x attributes=%02x packet=%d\n",ep.bEndpointAddress,ep.bmAttributes,ep.wMaxPacketSize);
                if((ep.bmAttributes&3)==2) {
                    if(ep.bEndpointAddress&0x80) in=ep.bEndpointAddress; else out=ep.bEndpointAddress;
                }
            }
        }
    }
    if(!console) { libusb_free_config_descriptor(cfg);libusb_close(h);libusb_free_device_list(list,1);libusb_exit(ctx);return 0; }
    if(control<0 || data<0 || !in || !out) return 1;
    if(active!=cfg->bConfigurationValue) {
        e=libusb_set_configuration(h,cfg->bConfigurationValue);
        std::fprintf(stderr,"set configuration: %s\n",libusb_error_name(e));
        if(e) return 1;
    }
    e=libusb_claim_interface(h,control);
    std::fprintf(stderr,"claim control: %s\n",libusb_error_name(e));
    if(e) return 1;
    e=libusb_claim_interface(h,data);
    std::fprintf(stderr,"claim data: %s\n",libusb_error_name(e));
    if(e) return 1;
    unsigned char coding[]={0x00,0xc2,0x01,0,0,0,8};
    e=libusb_control_transfer(h,0x21,0x20,0,control,coding,7,2000);
    std::fprintf(stderr,"line coding: %d\n",e);
    e=libusb_control_transfer(h,0x21,0x22,3,control,nullptr,0,2000);
    std::fprintf(stderr,"DTR/RTS: %d\n",e);
    auto start=std::chrono::steady_clock::now(); bool sent=false;
    while(std::chrono::steady_clock::now()-start<std::chrono::seconds(20)) {
        unsigned char buf[16384]; int got=0;
        e=libusb_bulk_transfer(h,in,buf,sizeof(buf),&got,200);
        if(got) { std::fwrite(buf,1,got,stdout);std::fflush(stdout); }
        if(e && e!=LIBUSB_ERROR_TIMEOUT) { std::fprintf(stderr,"read: %s\n",libusb_error_name(e));break; }
        if(!sent && std::chrono::steady_clock::now()-start>std::chrono::seconds(1)) {
            int count=0;
            e=libusb_bulk_transfer(h,out,(unsigned char*)argv[2],std::strlen(argv[2]),&count,2000);
            std::fprintf(stderr,"command: %s (%d bytes)\n",libusb_error_name(e),count);
            sent=true;
        }
    }
    libusb_control_transfer(h,0x21,0x22,0,control,nullptr,0,1000);
    libusb_release_interface(h,data);libusb_release_interface(h,control);
    libusb_free_config_descriptor(cfg);libusb_close(h);libusb_free_device_list(list,1);libusb_exit(ctx);
    return 0;
}
