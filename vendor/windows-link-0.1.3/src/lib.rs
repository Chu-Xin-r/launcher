#![no_std]

#[cfg(all(windows, target_env = "gnu"))]
#[macro_export]
macro_rules! __windows_gnu_link {
    ("advapi32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "advapi32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("bcrypt.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "bcrypt")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("comctl32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "comctl32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("comdlg32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "comdlg32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("crypt32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "crypt32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("gdi32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "gdi32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("iphlpapi.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "iphlpapi")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("kernel32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "kernel32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("ntdll.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "ntdll")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("ole32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "ole32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("oleaut32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "oleaut32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("psapi.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "psapi")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("rpcrt4.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "rpcrt4")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("secur32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "secur32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("setupapi.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "setupapi")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("shell32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "shell32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("user32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "user32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("userenv.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "userenv")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("winhttp.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "winhttp")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("winmm.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "winmm")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };
    ("ws2_32.dll" $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "ws2_32")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    };

    ($library:literal $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = "kernel32")]
        extern $abi {
            $(#[link_name=$link_name])?
            pub fn $($function)*;
        }
    };
}

#[cfg(all(windows, target_env = "gnu"))]
#[macro_export]
macro_rules! link {
    ($library:literal $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        $crate::__windows_gnu_link!($library $abi $($link_name)? fn $($function)*);
    }
}

#[cfg(all(windows, target_arch = "x86", not(target_env = "gnu")))]
#[macro_export]
macro_rules! link {
    ($library:literal $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = $library, kind = "raw-dylib", modifiers = "+verbatim", import_name_type = "undecorated")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    }
}

#[cfg(all(windows, not(target_arch = "x86"), not(target_env = "gnu")))]
#[macro_export]
macro_rules! link {
    ($library:literal $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        #[link(name = $library, kind = "raw-dylib", modifiers = "+verbatim")]
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    }
}

#[cfg(not(windows))]
#[macro_export]
macro_rules! link {
    ($library:literal $abi:literal $($link_name:literal)? fn $($function:tt)*) => {
        extern $abi { $(#[link_name=$link_name])? pub fn $($function)*; }
    }
}
