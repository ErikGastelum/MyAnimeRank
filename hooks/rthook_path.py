import os
import sys

if hasattr(sys, '_MEIPASS'):
    # Python 3.8+: proper way to register DLL search dirs on Windows 10/11.
    # PATH modification was deprecated for DLL loading (security change in Win10 1809).
    if hasattr(os, 'add_dll_directory'):
        os.add_dll_directory(sys._MEIPASS)
    # Keep PATH for any legacy ctypes usage
    os.environ['PATH'] = sys._MEIPASS + os.pathsep + os.environ.get('PATH', '')
