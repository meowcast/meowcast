#!/usr/bin/env python3
import unittest
from console_markers import frame_command,completed

class CompletionMarkers(unittest.TestCase):
    def test_wrapped_command_echo_is_not_execution(self):
        command,begin,end=frame_command('echo '+('x'*150),'0123456789abcdef')
        # Regression: the previous plain-text END marker could occupy a whole
        # terminal row, causing capture to exit while the GPU test still ran.
        echo=command.replace('USB_REPORT_END','\r\r\nUSB_REPORT_END').encode()
        self.assertFalse(completed(echo,begin,end))
        self.assertFalse(completed(echo+begin,begin,end))

    def test_previous_session_cannot_complete_new_session(self):
        _,begin,end=frame_command('true','0123456789abcdef')
        _,old_begin,old_end=frame_command('true','fedcba9876543210')
        self.assertFalse(completed(old_begin+old_end+begin,begin,end))
        self.assertTrue(completed(old_begin+old_end+begin+b'GPU result\n'+end,begin,end))

    def test_end_before_begin_is_not_completion(self):
        _,begin,end=frame_command('true','0123456789abcdef')
        self.assertFalse(completed(end+begin,begin,end))

if __name__=='__main__':unittest.main()
