"""Framed completion markers that cannot be forged by wrapped TTY echo."""
import re

def frame_command(payload, nonce):
    if not re.fullmatch(r'[0-9a-f]{16}',nonce):
        raise ValueError('Expected a fresh 16-digit hexadecimal nonce')
    begin=('\x1eUSB_SHELL_VERIFIED_'+nonce+'\x1f').encode()
    end=('\x1eUSB_REPORT_END_'+nonce+'\x1f').encode()
    command="\nprintf '\\036USB_SHELL_VERIFIED_"+nonce+"\\037\\n'; "+payload+"; printf '\\036USB_REPORT_END_"+nonce+"\\037\\n'\n"
    return command,begin,end

def completed(data, begin, end):
    start=data.find(begin)
    return start>=0 and data.find(end,start+len(begin))>=0
