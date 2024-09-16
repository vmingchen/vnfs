struct sum_args {
    int a;
    int b; 
};

/* commented out because xdrgen cannot handle the "program" section

program SUMPROG {
    version SUMVERS {
        int sum(sum_args) = 1;
    } = 1;
} = 22888;
*/
