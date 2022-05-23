
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'peerd' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'peerd'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'peerd' {
            [CompletionResult]::new('-L', 'L', [CompletionResultType]::ParameterName, 'Start daemon in listening mode binding the provided local address')
            [CompletionResult]::new('--listen', 'listen', [CompletionResultType]::ParameterName, 'Start daemon in listening mode binding the provided local address')
            [CompletionResult]::new('-C', 'C', [CompletionResultType]::ParameterName, 'Connect to a remote peer with the provided address after start')
            [CompletionResult]::new('--connect', 'connect', [CompletionResultType]::ParameterName, 'Connect to a remote peer with the provided address after start')
            [CompletionResult]::new('-p', 'p', [CompletionResultType]::ParameterName, 'Customize port used by lightning peer network')
            [CompletionResult]::new('--port', 'port', [CompletionResultType]::ParameterName, 'Customize port used by lightning peer network')
            [CompletionResult]::new('-o', 'o', [CompletionResultType]::ParameterName, 'Overlay peer communications through different transport protocol')
            [CompletionResult]::new('--overlay', 'overlay', [CompletionResultType]::ParameterName, 'Overlay peer communications through different transport protocol')
            [CompletionResult]::new('--peer-secret-key', 'peer-secret-key', [CompletionResultType]::ParameterName, 'peer-secret-key')
            [CompletionResult]::new('--token', 'token', [CompletionResultType]::ParameterName, 'Token used to authentify calls')
            [CompletionResult]::new('-d', 'd', [CompletionResultType]::ParameterName, 'Data directory path')
            [CompletionResult]::new('--data-dir', 'data-dir', [CompletionResultType]::ParameterName, 'Data directory path')
            [CompletionResult]::new('-T', 'T', [CompletionResultType]::ParameterName, 'Use Tor')
            [CompletionResult]::new('--tor-proxy', 'tor-proxy', [CompletionResultType]::ParameterName, 'Use Tor')
            [CompletionResult]::new('-m', 'm', [CompletionResultType]::ParameterName, 'ZMQ socket name/address to forward all incoming protocol messages')
            [CompletionResult]::new('--msg-socket', 'msg-socket', [CompletionResultType]::ParameterName, 'ZMQ socket name/address to forward all incoming protocol messages')
            [CompletionResult]::new('-x', 'x', [CompletionResultType]::ParameterName, 'ZMQ socket name/address for daemon control interface')
            [CompletionResult]::new('--ctl-socket', 'ctl-socket', [CompletionResultType]::ParameterName, 'ZMQ socket name/address for daemon control interface')
            [CompletionResult]::new('-h', 'h', [CompletionResultType]::ParameterName, 'Print help information')
            [CompletionResult]::new('--help', 'help', [CompletionResultType]::ParameterName, 'Print help information')
            [CompletionResult]::new('-V', 'V', [CompletionResultType]::ParameterName, 'Print version information')
            [CompletionResult]::new('--version', 'version', [CompletionResultType]::ParameterName, 'Print version information')
            [CompletionResult]::new('-v', 'v', [CompletionResultType]::ParameterName, 'Set verbosity level')
            [CompletionResult]::new('--verbose', 'verbose', [CompletionResultType]::ParameterName, 'Set verbosity level')
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
