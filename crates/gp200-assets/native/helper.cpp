// Owned Linux process boundary for the MIT offline kernel. No MIDI/network API.
#include "trainer.inc"
#include "clo_refiner.hpp"
#include <iostream>
#include <stdexcept>
#include <sys/resource.h>
#include <sys/prctl.h>
#include <unistd.h>
#include <signal.h>

namespace ntc {
std::string pathToUtf8(const fs::path& p) { return p.string(); }
bool readFileBytes(const fs::path& p,std::vector<std::uint8_t>& out,std::string& error) {
    std::ifstream f(p,std::ios::binary|std::ios::ate);
    if(!f){error="Cannot open helper input";return false;}
    auto n=f.tellg();if(n<0 || n>64*1024*1024){error="Helper input exceeds 64 MiB";return false;}
    out.resize(static_cast<std::size_t>(n));f.seekg(0);
    f.read(reinterpret_cast<char*>(out.data()),n);return bool(f);
}
bool writeFileBytes(const fs::path& p,const std::uint8_t* b,std::size_t n,std::string& error) {
    if(n>64*1024*1024){error="Helper output exceeds 64 MiB";return false;}
    std::ofstream f(p,std::ios::binary|std::ios::trunc);
    f.write(reinterpret_cast<const char*>(b),static_cast<std::streamsize>(n));
    if(!f){error="Cannot write helper output";return false;}return true;
}
std::vector<float> readSamples(const fs::path& path,std::size_t expected) {
    std::vector<std::uint8_t> b;std::string error;
    if(!readFileBytes(path,b,error) || b.size()!=expected*4)throw std::runtime_error("Invalid sample input length");
    std::vector<float> samples(expected);
    for(std::size_t i=0;i<expected;++i){std::uint32_t u=le32(b.data()+4*i);std::memcpy(&samples[i],&u,4);if(!std::isfinite(samples[i]))throw std::runtime_error("Non-finite input");}
    return samples;
}
void writeSamples(const fs::path& path,const std::vector<float>& samples) {
    std::vector<std::uint8_t> b(samples.size()*4);
    for(std::size_t i=0;i<samples.size();++i){if(!std::isfinite(samples[i]))throw std::runtime_error("Non-finite resampler output");putFloat(b,4*i,samples[i]);}
    std::string error;if(!writeFileBytes(path,b.data(),b.size(),error))throw std::runtime_error(error);
}
}

int main(int argc,char** argv)try {
    if(argc!=5)throw std::runtime_error("Expected mode, work directory, rate, parent PID");
    // Stop on abrupt application exit as well as orderly worker cancellation.
    // Checking the original parent after installing the signal closes the
    // exec/startup race if the Rust parent exited before this process started.
    if(prctl(PR_SET_PDEATHSIG,SIGKILL) || getppid()!=static_cast<pid_t>(std::stoul(argv[4])))
        throw std::runtime_error("Calibration parent is unavailable");
    // Process memory and CPU are bounded independently of the Rust wall timeout.
    struct rlimit memory{2ull*1024*1024*1024,2ull*1024*1024*1024};
    struct rlimit cpu{900,900};
    if(setrlimit(RLIMIT_AS,&memory) || setrlimit(RLIMIT_CPU,&cpu))throw std::runtime_error("Cannot apply helper resource limits");
    const std::string mode=argv[1];const ntc::fs::path work=argv[2];
    const std::string rateText=argv[3];
    if(rateText!="44100"&&rateText!="48000"&&rateText!="96000")throw std::runtime_error("Unverified trainer rate");
    const unsigned rate=std::stoul(rateText);
    constexpr std::size_t n44=70u*44100u;const std::size_t n=70u*rate;
    auto source=ntc::readSamples(work/"source.f32",n44);
    if(mode=="resample") {
        auto input=ntc::resampleR8Brain24(source,44100.0,rate);input.resize(n,0.0f);
        ntc::writeSamples(work/"input.f32",input);return 0;
    }
    if(mode!="calibrate")throw std::runtime_error("Unknown helper mode");
    auto input=ntc::readSamples(work/"input.f32",n);
    auto target=ntc::readSamples(work/"target.f32",n);
    input.resize(n+600,0.0f);target.resize(n+600,0.0f);
    auto toneTarget=ntc::prepareToneTarget44100(target,rate);
    ntc::detrend(target);const auto latency=ntc::detectLatency(target,rate);target=ntc::alignLeft(target,latency);
    ntc::Model model;model.A.assign(128,0);model.A[0]=1;model.B.assign(2048,0);model.B[0]=1;
    model.pk=ntc::fitPk(input,target,rate);model.pre=ntc::Biquad{};model.post=ntc::postForRate(rate);
    if(!(model.pk.pp>0 && model.pk.pn>0 && model.pk.kp>0 && model.pk.kn>0) ||
       !std::isfinite(model.pk.pp) || !std::isfinite(model.pk.pn) || !std::isfinite(model.pk.kp) || !std::isfinite(model.pk.kn))
       throw std::runtime_error("Trainer requires finite positive/negative excitation and response");
    const auto status=[](const std::wstring& s){std::string line(s.begin(),s.end());std::cerr<<line<<"\n";};
    ntc::fitAB(model,input,target,rate,status);ntc::refineB(model,input,target,rate,status);
    std::string error;
    if(!ntc::serialize2048(work/"intermediate.clo",model,rate,error))throw std::runtime_error(error);
    source.resize(n44+600,0.0f);
    if(!ntc::writeMonoFloat32Wav(work/"source.wav",source,44100,error) ||
       !ntc::writeMonoFloat32Wav(work/"target.wav",toneTarget,44100,error))throw std::runtime_error(error);
    ntc::CloRefineStats stats;ntc::CloRefineConfig config;
    if(!ntc::refineCloBOnly(work/"intermediate.clo",work/"source.wav",work/"target.wav",work/"final.clo",config,error,status,&stats))throw std::runtime_error(error);
    std::ofstream report(work/"metrics.txt");report.precision(17);
    report<<latency<<"\n"<<stats.finalRmseDb<<"\n"<<stats.analysisFrames<<"\n";
    if(!report)throw std::runtime_error("Cannot write metrics");
    return 0;
}catch(const std::exception& e){std::cerr<<"Calibration failed: "<<e.what()<<"\n";return 2;}
